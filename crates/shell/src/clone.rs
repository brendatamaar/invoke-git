//! Runs the core's clone jobs (`CloneCommand`) with the system git.
//!
//! The same child-process machinery as `RunGit` (`runner::run_child`: pipes,
//! progress parsing, process-tree cancel) and the same classifier. On top:
//!
//! - the helper binary is registered as `GIT_ASKPASS` / `SSH_ASKPASS`, and
//!   each prompt becomes `CloneEvent::AuthNeeded`
//! - a clone that does not succeed leaves nothing behind: the folder is
//!   removed if the clone created it, or emptied if it was an empty folder
//!   that already existed. Leading folders the clone created go too.
//!
//! Every job ends with exactly one `CloneEvent::Finished`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use invoke_core::clone::{CloneCommand, CloneEvent, CloneRequest};
use invoke_core::{AuthAnswer, ErrorClass, Failure, OpId, OpOutcome};

use crate::askpass::AskpassServer;
use crate::classify::classify;
use crate::git::Git;
use crate::runner::{Control, ProgressSink, failed, lock, run_child};

struct Entry {
    control: Arc<Control>,
    askpass: Mutex<Option<Arc<AskpassServer>>>,
}

pub struct CloneRunner {
    git: Git,
    /// The `askpass` helper binary shipped next to the app.
    askpass_program: PathBuf,
    env: Vec<(OsString, OsString)>,
    jobs: Arc<Mutex<HashMap<OpId, Arc<Entry>>>>,
}

impl CloneRunner {
    pub fn new(git: Git, askpass_program: impl Into<PathBuf>) -> Self {
        Self {
            git,
            askpass_program: askpass_program.into(),
            env: Vec::new(),
            jobs: Arc::default(),
        }
    }

    /// Extra environment for every clone (tests use it to isolate config).
    pub fn with_env(mut self, env: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        self.env.extend(env);
        self
    }

    /// Carries out one command from `step_clone`.
    pub fn execute(&self, command: CloneCommand, events: Sender<CloneEvent>) {
        match command {
            CloneCommand::Start { op, request } => self.run(op, request, events),
            CloneCommand::Cancel(op) => self.cancel(op),
            CloneCommand::AnswerAuth(op, answer) => self.answer(op, answer),
        }
    }

    /// Starts one clone in the background. The last event for `op` is always
    /// `Finished`.
    pub fn run(&self, op: OpId, request: CloneRequest, events: Sender<CloneEvent>) {
        let entry = Arc::new(Entry {
            control: Arc::default(),
            askpass: Mutex::new(None),
        });
        lock(&self.jobs).insert(op, Arc::clone(&entry));
        let job = Job {
            op,
            request,
            git: self.git.clone(),
            program: self.askpass_program.clone(),
            env: self.env.clone(),
            entry,
            events: events.clone(),
        };
        let jobs = Arc::clone(&self.jobs);
        let spawned = thread::Builder::new()
            .name(format!("clone {}", op.0))
            .spawn(move || {
                let events = job.events.clone();
                let outcome = job.execute();
                lock(&jobs).remove(&op);
                let _ = events.send(CloneEvent::Finished(op, outcome));
            });
        if let Err(error) = spawned {
            lock(&self.jobs).remove(&op);
            let _ = events.send(CloneEvent::Finished(
                op,
                failed(ErrorClass::Environment, format!("start thread: {error}")),
            ));
        }
    }

    /// Cancels a clone. Unknown or finished jobs are ignored.
    pub fn cancel(&self, op: OpId) {
        if let Some(entry) = lock(&self.jobs).get(&op) {
            entry.control.cancel();
        }
    }

    /// Passes the user's answer to the helper waiting for it.
    pub fn answer(&self, op: OpId, answer: AuthAnswer) {
        let entry = lock(&self.jobs).get(&op).cloned();
        if let Some(entry) = entry
            && let Some(server) = lock(&entry.askpass).as_ref()
        {
            server.answer(answer);
        }
    }
}

struct Job {
    op: OpId,
    request: CloneRequest,
    git: Git,
    program: PathBuf,
    env: Vec<(OsString, OsString)>,
    entry: Arc<Entry>,
    events: Sender<CloneEvent>,
}

impl Job {
    fn execute(self) -> OpOutcome {
        if self.entry.control.is_cancelled() {
            return OpOutcome::Cancelled;
        }
        let target = self.request.target.clone();
        if !target.is_absolute() {
            return refused(format!(
                "The folder must be an absolute path: {}",
                target.display()
            ));
        }
        if let Some(reason) = occupied(&target) {
            return refused(reason);
        }
        // Remember what exists now, so cleanup removes only what the clone
        // creates.
        let created = first_missing(&target);
        let existed_empty = target.is_dir();
        let workdir = nearest_existing(&target);

        let (op, events) = (self.op, self.events.clone());
        let server = match AskpassServer::start(
            op,
            Arc::new(move |prompt| {
                let _ = events.send(CloneEvent::AuthNeeded(op, prompt));
            }),
        ) {
            Ok(server) => Arc::new(server),
            Err(error) => {
                return failed(ErrorClass::Environment, format!("start askpass: {error}"));
            }
        };
        *lock(&self.entry.askpass) = Some(Arc::clone(&server));

        let mut command = self.git.command(&workdir);
        command
            .args(crate::git::GLOBAL_ARGS)
            // `ext::` transports run arbitrary commands; never from a URL
            // the user pasted.
            .args(["-c", "protocol.ext.allow=never"])
            .args(["clone", "--progress", "--"])
            .arg(&self.request.url)
            .arg(&target)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .envs(server.env(&self.program));
        let (op, events) = (self.op, self.events.clone());
        let sink: ProgressSink = Box::new(move |update| {
            let _ = events.send(CloneEvent::Progress(op, update));
        });
        let outcome = match run_child(command, None, Some(sink), &self.entry.control) {
            Ok(done) if done.killed && !done.success => OpOutcome::Cancelled,
            Ok(done) => classify(done.success, &done.output),
            Err(error) => failed(ErrorClass::Environment, format!("start git: {error}")),
        };
        server.shutdown();
        lock(&self.entry.askpass).take();
        if outcome != OpOutcome::Succeeded {
            clean_up(&target, created.as_deref(), existed_empty);
        }
        outcome
    }
}

fn refused(details: String) -> OpOutcome {
    OpOutcome::Failed(Failure {
        class: ErrorClass::Expected,
        details,
    })
}

/// Why git would refuse the target: it exists and is not an empty folder.
fn occupied(target: &Path) -> Option<String> {
    match std::fs::read_dir(target) {
        Ok(mut entries) => entries.next().is_some().then(|| {
            format!(
                "destination path '{}' already exists and is not an empty directory",
                target.display()
            )
        }),
        Err(_) if target.exists() => Some(format!(
            "destination path '{}' already exists and is not a directory",
            target.display()
        )),
        Err(_) => None,
    }
}

/// The topmost ancestor of `path` (or `path` itself) that does not exist yet.
/// `None` when `path` exists.
fn first_missing(path: &Path) -> Option<PathBuf> {
    let mut missing = None;
    let mut current = path;
    while !current.exists() {
        missing = Some(current.to_path_buf());
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    missing
}

fn nearest_existing(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|ancestor| ancestor.is_dir())
        .map_or_else(std::env::temp_dir, Path::to_path_buf)
}

/// Removes what a failed or cancelled clone left. `created` is the topmost
/// folder the clone made, or `None` when the target was an existing empty
/// folder, which is emptied but kept.
fn clean_up(target: &Path, created: Option<&Path>, existed_empty: bool) {
    match created {
        Some(created) => retry(|| remove_all(created)),
        None if existed_empty => retry(|| empty(target)),
        None => {}
    }
}

fn remove_all(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn empty(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() && !path.is_symlink() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Windows can hold a just-killed process's files for a moment.
fn retry(mut action: impl FnMut() -> std::io::Result<()>) {
    for _ in 0..20 {
        if action().is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test, so the scratch folder is never used concurrently.
    #[test]
    fn cleanup_removes_only_what_the_clone_created() {
        let root = std::env::temp_dir().join(format!("invoke-cleanup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // The clone created `a/b/repo`: all of it goes, nothing above it.
        let target = root.join("a").join("b").join("repo");
        let created = first_missing(&target);
        assert_eq!(created.as_deref(), Some(root.join("a").as_path()));
        std::fs::create_dir_all(target.join("inner")).unwrap();
        clean_up(&target, created.as_deref(), false);
        assert!(!root.join("a").exists());
        assert!(root.exists());

        // An empty folder that already existed is emptied but kept.
        let target = root.join("repo");
        std::fs::create_dir(&target).unwrap();
        assert_eq!(first_missing(&target), None);
        std::fs::create_dir_all(target.join("inner").join("deeper")).unwrap();
        std::fs::write(target.join("f.txt"), "x").unwrap();
        clean_up(&target, None, true);
        assert!(target.is_dir());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);

        // Occupied targets are detected.
        assert_eq!(occupied(&root.join("new")), None);
        assert_eq!(occupied(&target), None);
        std::fs::write(target.join("f"), "x").unwrap();
        assert!(occupied(&target).is_some());
        assert!(occupied(&target.join("f")).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }
}
