//! Runs the core's `RunGit` commands with the system git, one thread per
//! operation, and reports each as `OpProgress` events and exactly one
//! `OpFinished`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use invoke_core::{
    AuthAnswer, ErrorClass, Event, Expect, Failure, GitArgs, OpId, OpOutcome, Progress,
};

use crate::askpass::AskpassServer;
use crate::classify::{classify, classify_commit, classify_stash_save};
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
    /// The `askpass` helper binary; set, network operations can prompt
    /// (task 17, reusing task 16's server).
    askpass_program: Option<PathBuf>,
    ops: Arc<Mutex<HashMap<OpId, Arc<Control>>>>,
}

/// Cancel flag shared between the caller and the thread running git.
#[derive(Default)]
pub(crate) struct Control {
    cancelled: AtomicBool,
    /// The credential prompt server of a running network operation.
    askpass: Mutex<Option<Arc<AskpassServer>>>,
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
            askpass_program: None,
            ops: Arc::default(),
        }
    }

    /// Extra environment for every git run (tests use it to isolate config).
    pub fn with_env(mut self, env: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        self.env.extend(env);
        self
    }

    /// Lets fetch, pull and push ask for credentials in the app: the helper
    /// at `program` is registered as the askpass program, and each prompt
    /// becomes `Event::AuthNeeded`.
    pub fn with_askpass(mut self, program: impl Into<PathBuf>) -> Self {
        self.askpass_program = Some(program.into());
        self
    }

    /// Passes the user's answer to the helper waiting for it
    /// (`Command::AnswerAuth`).
    pub fn answer(&self, op: OpId, answer: AuthAnswer) {
        let control = lock(&self.ops).get(&op).cloned();
        if let Some(control) = control
            && let Some(server) = lock(&control.askpass).as_ref()
        {
            server.answer(answer);
        }
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
            askpass_program: self.askpass_program.clone(),
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
    askpass_program: Option<PathBuf>,
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
        // Rule 5 for refs: the branches the run names must still be where
        // the core saw them (task 15).
        if let Err(failure) = precondition::check_refs(&self.paths, &self.args.expected_refs()) {
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
        let mut server = None;
        if let Some(program) = self
            .askpass_program
            .as_ref()
            .filter(|_| self.args.is_network())
        {
            let events = self.events.clone();
            match AskpassServer::start(
                op,
                Arc::new(move |prompt| {
                    let _ = events.send(Event::AuthNeeded(op, prompt));
                }),
            ) {
                Ok(started) => {
                    let started = Arc::new(started);
                    command.envs(started.env(program));
                    *lock(&self.control.askpass) = Some(Arc::clone(&started));
                    server = Some(started);
                }
                Err(error) => {
                    return failed(ErrorClass::Environment, format!("start askpass: {error}"));
                }
            }
        }
        // Task 14: git says nothing of its own when a commit hook fails, so
        // a commit also writes its trace2 events, which name the hook.
        let trace = matches!(self.args, GitArgs::Commit { .. }).then(|| TraceFile::new(op));
        if let Some(trace) = &trace {
            command.env("GIT_TRACE2_EVENT", &trace.path);
        }
        let outcome = match run_child(command, run.stdin, Some(sink), &self.control) {
            Ok(done) if done.killed && !done.success => OpOutcome::Cancelled,
            // A cancel that lost the race to a finished git reports what
            // really happened.
            Ok(done) => match self.args {
                GitArgs::StashSave { .. } => classify_stash_save(done.success, &done.output),
                GitArgs::Commit { .. } => {
                    let events = trace.as_ref().map(TraceFile::read).unwrap_or_default();
                    classify_commit(done.success, &done.output, &events)
                }
                _ => classify(done.success, &done.output),
            },
            Err(error) => failed(ErrorClass::Environment, format!("start git: {error}")),
        };
        if let Some(server) = server {
            server.shutdown();
            lock(&self.control.askpass).take();
        }
        outcome
    }
}

/// Where git writes its trace2 events for one run (`GIT_TRACE2_EVENT`), in
/// the temp folder (never in `.git`, which only our writes touch). Gone when
/// dropped.
struct TraceFile {
    path: PathBuf,
}

/// The most trace read back: a hook that runs a lot of git cannot make the
/// shell read without bound.
const TRACE_LIMIT: u64 = 8 * 1024 * 1024;

impl TraceFile {
    fn new(op: OpId) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "invoke-git-trace2-{}-{}-{}.jsonl",
            std::process::id(),
            op.0,
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    /// What git wrote; empty when it wrote nothing or it cannot be read
    /// (the commit is then classified from its output alone).
    fn read(&self) -> String {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return String::new();
        };
        let mut bytes = Vec::new();
        let _ = file.take(TRACE_LIMIT).read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Drop for TraceFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
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
