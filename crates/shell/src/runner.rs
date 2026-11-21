//! Runs the core's `RunGit` commands with the system git, one thread per
//! operation, and reports each as `OpProgress` events and exactly one
//! `OpFinished`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use invoke_core::{ErrorClass, Event, Expect, Failure, GitArgs, OpId, OpOutcome, Progress};

use crate::classify::{classify, classify_stash_save};
use crate::git::{Git, invocation};
use crate::precondition::{self, RepoPaths};
use crate::progress::{self, Lines};
use crate::tree::{self, ProcessTree};

/// How long git gets to exit after SIGTERM before SIGKILL (Unix).
const GRACE: Duration = Duration::from_secs(2);
/// How often a running op checks for exit and cancel.
const POLL: Duration = Duration::from_millis(20);
/// How long to wait for stdout and stderr to close after git exits. A
/// daemon git left running on purpose may hold them open forever.
const DRAIN: Duration = Duration::from_millis(500);
/// Output kept for the "Details" toggle: the last 64 KiB of each stream.
const KEEP: usize = 64 * 1024;

pub struct GitRunner {
    git: Git,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
    ops: Arc<Mutex<HashMap<OpId, Arc<Control>>>>,
}

/// Cancel flag shared between the caller and the thread running git.
#[derive(Default)]
pub(crate) struct Control {
    cancelled: AtomicBool,
}

impl Control {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Called for each parsed progress line of stderr.
pub(crate) type ProgressSink = Box<dyn Fn(Progress) + Send>;

/// How a spawned git ended.
pub(crate) struct Finished {
    pub success: bool,
    /// A cancel was sent (it may have lost the race to a normal exit).
    pub killed: bool,
    /// stdout then stderr, the last 64 KiB of each.
    pub output: String,
}

impl GitRunner {
    pub fn new(git: Git, paths: RepoPaths) -> Self {
        Self {
            git,
            paths,
            env: Vec::new(),
            ops: Arc::default(),
        }
    }

    /// Extra environment for every git run (tests use it to isolate config).
    pub fn with_env(mut self, env: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        self.env.extend(env);
        self
    }

    /// Starts one operation in the background. Events go to `events`; the
    /// last one for `op` is always `OpFinished`.
    pub fn run(&self, op: OpId, args: GitArgs, expect: Expect, events: Sender<Event>) {
        let control = Arc::new(Control::default());
        lock(&self.ops).insert(op, Arc::clone(&control));
        let job = Job {
            op,
            args,
            expect,
            git: self.git.clone(),
            paths: self.paths.clone(),
            env: self.env.clone(),
            control,
            events: events.clone(),
        };
        let ops = Arc::clone(&self.ops);
        let spawned = thread::Builder::new()
            .name(format!("git op {}", op.0))
            .spawn(move || {
                let events = job.events.clone();
                let outcome = job.execute();
                lock(&ops).remove(&op);
                let _ = events.send(Event::OpFinished(op, outcome));
            });
        if let Err(error) = spawned {
            lock(&self.ops).remove(&op);
            let _ = events.send(Event::OpFinished(
                op,
                failed(ErrorClass::Environment, format!("start thread: {error}")),
            ));
        }
    }

    /// Cancels an operation. Unknown or finished ops are ignored: the core
    /// already treats a late cancel as a no-op.
    pub fn cancel(&self, op: OpId) {
        if let Some(control) = lock(&self.ops).get(&op) {
            control.cancel();
        }
    }
}

struct Job {
    op: OpId,
    args: GitArgs,
    expect: Expect,
    git: Git,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
    control: Arc<Control>,
    events: Sender<Event>,
}

impl Job {
    fn cancelled(&self) -> bool {
        self.control.is_cancelled()
    }

    fn execute(self) -> OpOutcome {
        if self.cancelled() {
            return OpOutcome::Cancelled;
        }
        if let Err(failure) = precondition::check(&self.paths, &self.expect) {
            return OpOutcome::Failed(failure);
        }
        // Task 20: a stash is addressed by id; refuse if it moved (rule 5).
        if let Some((index, id)) = self.args.stash_target()
            && let Err(failure) = precondition::check_stash(&self.paths, index, id)
        {
            return OpOutcome::Failed(failure);
        }
        let run = invocation(&self.args);
        let mut command = self.git.command(&self.paths.workdir);
        command
            .args(&run.args)
            .envs(self.env.iter().map(|(key, value)| (key, value)));
        let (op, events) = (self.op, self.events.clone());
        let sink: ProgressSink = Box::new(move |update| {
            let _ = events.send(Event::OpProgress(op, update));
        });
        match run_child(command, run.stdin, Some(sink), &self.control) {
            Ok(done) if done.killed && !done.success => OpOutcome::Cancelled,
            // A cancel that lost the race to a finished git reports what
            // really happened.
            Ok(done) => match self.args {
                GitArgs::StashSave { .. } => classify_stash_save(done.success, &done.output),
                _ => classify(done.success, &done.output),
            },
            Err(error) => failed(ErrorClass::Environment, format!("start git: {error}")),
        }
    }
}

/// Runs a prepared git command to its end: pipes, process tree, progress
/// parsing, cancel. Shared by `RunGit` ops and clones.
pub(crate) fn run_child(
    mut command: Command,
    stdin: Option<Vec<u8>>,
    progress: Option<ProgressSink>,
    control: &Control,
) -> std::io::Result<Finished> {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    tree::prepare(&mut command);
    let mut child = command.spawn()?;
    let tree = ProcessTree::attach(&child);

    if let (Some(mut pipe), Some(bytes)) = (child.stdin.take(), stdin) {
        // A thread, so a git that writes a lot before reading cannot
        // deadlock against us.
        thread::spawn(move || {
            let _ = pipe.write_all(&bytes);
        });
    }
    let stdout = drain(child.stdout.take(), None);
    let stderr = drain(child.stderr.take(), progress);

    let (success, killed) = wait(control, &mut child, &tree);
    let mut output = collect(stdout);
    output.push_str(&collect(stderr));
    Ok(Finished {
        success,
        killed,
        output,
    })
}

/// Waits for git, terminating the tree on cancel and killing it if it
/// outlives the grace period. Returns (exit success, cancel was sent).
fn wait(control: &Control, child: &mut Child, tree: &ProcessTree) -> (bool, bool) {
    let mut terminated_at: Option<Instant> = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.success(), terminated_at.is_some()),
            Ok(None) => {}
            Err(_) => {
                tree.kill(child);
                return (false, terminated_at.is_some());
            }
        }
        match terminated_at {
            None if control.is_cancelled() => {
                tree.terminate(child);
                terminated_at = Some(Instant::now());
            }
            Some(at) if at.elapsed() >= GRACE => tree.kill(child),
            _ => {}
        }
        thread::sleep(POLL);
    }
}

type Drained = mpsc::Receiver<Vec<u8>>;

/// Reads a stream to its end on its own thread, keeping the tail. With an
/// op and sink, also turns progress lines into `OpProgress` events.
fn drain(stream: Option<impl Read + Send + 'static>, progress: Option<ProgressSink>) -> Drained {
    let (done, result) = mpsc::channel();
    if let Some(mut stream) = stream {
        thread::spawn(move || {
            let mut kept = Vec::new();
            let mut lines = Lines::default();
            let mut buffer = [0u8; 8192];
            loop {
                let read = match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                let chunk = &buffer[..read];
                kept.extend_from_slice(chunk);
                if kept.len() > 2 * KEEP {
                    kept.drain(..kept.len() - KEEP);
                }
                if let Some(sink) = &progress {
                    for line in lines.push(chunk) {
                        if let Some(update) = progress::parse(&line) {
                            sink(update);
                        }
                    }
                }
            }
            if kept.len() > KEEP {
                kept.drain(..kept.len() - KEEP);
            }
            let _ = done.send(kept);
        });
    }
    result
}

fn collect(drained: Drained) -> String {
    let bytes = drained.recv_timeout(DRAIN).unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(crate) fn failed(class: ErrorClass, details: String) -> OpOutcome {
    OpOutcome::Failed(Failure { class, details })
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
