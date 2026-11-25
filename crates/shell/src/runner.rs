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
use crate::git::{Git, Invocation, invocations};
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
    rebase_editor: Option<PathBuf>,
    ops: Arc<Mutex<HashMap<OpId, Arc<Control>>>>,
}

/// Cancel flag shared between the caller and the thread running git.
#[derive(Default)]
pub(crate) struct Control {
    cancelled: AtomicBool,
    defer_cancellation: bool,
    /// The credential prompt server of a running network operation.
    askpass: Mutex<Option<Arc<AskpassServer>>>,
}

impl Control {
    pub(crate) fn cancel(&self) {
        if !self.defer_cancellation {
            self.cancelled.store(true, Ordering::SeqCst);
        }
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
            rebase_editor: crate::rebase::default_editor(),
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

    /// Uses a native editor executable with a shell-safe basename.
    pub fn with_rebase_editor(mut self, program: impl Into<PathBuf>) -> Self {
        self.rebase_editor = Some(program.into());
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
        let control = Arc::new(Control {
            defer_cancellation: crate::rebase::is_sequencer(&args),
            ..Control::default()
        });
        lock(&self.ops).insert(op, Arc::clone(&control));
        let job = Job {
            op,
            args,
            expect,
            git: self.git.clone(),
            paths: self.paths.clone(),
            env: self.env.clone(),
            askpass_program: self.askpass_program.clone(),
            rebase_editor: self.rebase_editor.clone(),
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
    rebase_editor: Option<PathBuf>,
    control: Arc<Control>,
    events: Sender<Event>,
}

impl Job {
    fn cancelled(&self) -> bool {
        self.control.is_cancelled()
    }

    /// Runs `git lfs version` through the cancellable child runner (`run_child`).
    /// Returns `None` when `git-lfs` is available, or an early `OpOutcome`
    /// (`Cancelled` or `Failed(Environment)`).
    fn probe_git_lfs(&self) -> Option<OpOutcome> {
        let mut command = self.git.command(&self.paths.workdir);
        command
            .args(["lfs", "version"])
            .envs(self.env.iter().map(|(key, value)| (key, value)));
        match run_child(command, None, None, &self.control) {
            Ok(done) if done.killed && !done.success => Some(OpOutcome::Cancelled),
            Ok(done) if done.success && done.output.trim_start().starts_with("git-lfs/") => None,
            Ok(_) => Some(failed(
                ErrorClass::Environment,
                "Git LFS is not installed (`git-lfs` was not found on PATH). Install Git LFS to pull LFS objects.".into(),
            )),
            Err(error) => Some(failed(
                ErrorClass::Environment,
                format!("start git: {error}"),
            )),
        }
    }

    /// Before a non-forced `git worktree remove`, checks the target worktree for
    /// ignored, untracked, or modified files (`git worktree remove` alone ignores
    /// gitignored files like `.env` and deletes them silently).
    fn check_worktree_ignored_or_dirty(
        &self,
        workdir: &std::path::Path,
        display_path: &str,
    ) -> Option<OpOutcome> {
        let mut command = self.git.command(workdir);
        command
            .args(crate::git::GLOBAL_ARGS)
            .args([
                "status",
                "--porcelain=v1",
                "--ignored=matching",
                "--untracked-files=all",
                "-z",
            ])
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            // Never refresh the target worktree's index during the read-only
            // cleanliness probe, or the post-probe index guard would see drift.
            .env("GIT_OPTIONAL_LOCKS", "0");
        match run_child(command, None, None, &self.control) {
            Ok(done) if done.killed && !done.success => Some(OpOutcome::Cancelled),
            Ok(done) if !done.success => Some(failed(
                ErrorClass::Expected,
                format!(
                    "Could not verify whether worktree '{display_path}' contains ignored or uncommitted files: {}",
                    done.output.trim()
                ),
            )),
            Ok(done) => {
                let paths = crate::advanced::parse_porcelain_z_paths(&done.output);
                if paths.is_empty() {
                    return None;
                }
                let limit = 10;
                let shown = paths
                    .iter()
                    .take(limit)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let suffix = if paths.len() > limit {
                    format!(", and {} more", paths.len() - limit)
                } else {
                    String::new()
                };
                Some(failed(
                    ErrorClass::Expected,
                    format!(
                        "Refused to remove worktree '{display_path}': it contains ignored, untracked, or modified files that would be lost ({shown}{suffix}). Move them or tick Force remove."
                    ),
                ))
            }
            Err(error) => Some(failed(
                ErrorClass::Environment,
                format!("start git: {error}"),
            )),
        }
    }

    fn execute(self) -> OpOutcome {
        if self.cancelled() {
            return OpOutcome::Cancelled;
        }
        if let Err(failure) = precondition::check(&self.paths, &self.expect) {
            return OpOutcome::Failed(failure);
        }
        match &self.args {
            GitArgs::AddPath(path) | GitArgs::RestoreStagedPath(path) => {
                return crate::staging::StagingRunner::new(self.git, self.paths)
                    .with_env(self.env)
                    .apply_path(&self.args, path, &self.expect);
            }
            GitArgs::DiscardPath { path, file } => {
                return crate::discard::DiscardRunner::new(self.git, self.paths)
                    .with_env(self.env)
                    .discard_path(self.op, &self.args, path, &self.expect, file.as_ref());
            }
            _ => {}
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
        if let Err(failure) = crate::rebase::check(&self.paths, &self.args, &self.expect) {
            return OpOutcome::Failed(failure);
        }
        if let GitArgs::WorktreeRemove {
            path,
            force,
            expect_head,
            expect_index,
            ..
        } = &self.args
        {
            let target = match crate::advanced::check_worktree_remove_target(
                &self.paths,
                path,
                expect_head.as_ref(),
                expect_index.as_ref(),
                *force,
            ) {
                Ok(t) => t,
                Err(failure) => return OpOutcome::Failed(failure),
            };
            // Re-checked on disk at run time: if the checkout folder is truly
            // missing, fall through to `git worktree remove -- <path>` (without
            // `--force`), which removes only this target's admin entry without
            // pruning unrelated missing worktrees.
            if target.dir_exists && !*force {
                if !target.has_git_link {
                    return failed(
                        ErrorClass::Expected,
                        format!(
                            "Refused to remove worktree '{path}': directory exists on disk without a valid .git link. Inspect its contents or tick Force remove."
                        ),
                    );
                }
                if let Some(early) = self.check_worktree_ignored_or_dirty(&target.workdir, path) {
                    return early;
                }
                if self.cancelled() {
                    return OpOutcome::Cancelled;
                }
                if let Err(failure) = precondition::check(&self.paths, &self.expect) {
                    return OpOutcome::Failed(failure);
                }
                if let Err(failure) = crate::advanced::check_worktree_remove_target(
                    &self.paths,
                    path,
                    expect_head.as_ref(),
                    expect_index.as_ref(),
                    *force,
                ) {
                    return OpOutcome::Failed(failure);
                }
            }
        }
        if let GitArgs::Submodule {
            op: invoke_core::SubmoduleOp::UpdateInitRecursive,
            path,
        } = &self.args
        {
            if let Err(failure) = run_submodule_preflight(
                &self.paths,
                &self.expect,
                path.as_ref(),
                crate::advanced::check_submodule_update,
            ) {
                return OpOutcome::Failed(failure);
            }
            if self.cancelled() {
                return OpOutcome::Cancelled;
            }
        }
        // P10: run the `git lfs version` probe through the cancellable runner,
        // then re-check the final guard (`index.lock`, HEAD, index) right
        // before launching `git lfs pull`.
        if let GitArgs::LfsPull { path } = &self.args {
            if let Some(p) = path
                && !invoke_core::lfs_include_path_is_literal(p)
            {
                return failed(
                    ErrorClass::Expected,
                    format!(
                        "Cannot pull '{}' individually because its path contains comma or wildcard characters. Use LFS Pull (all) instead.",
                        p.0
                    ),
                );
            }
            if let Some(early) = self.probe_git_lfs() {
                return early;
            }
            if self.cancelled() {
                return OpOutcome::Cancelled;
            }
            if let Err(failure) = precondition::check(&self.paths, &self.expect) {
                return OpOutcome::Failed(failure);
            }
        }
        if let GitArgs::CheckoutPr {
            remote,
            remote_ref,
            local_branch,
        } = &self.args
        {
            return self.run_checkout_pr(remote, &remote_ref.0, &local_branch.0);
        }
        // Task 23: a request is one git run, except taking a side whole,
        // which is two; it stops at the first one that fails.
        let mut outcome = OpOutcome::Succeeded;
        for run in invocations(&self.args) {
            if self.cancelled() {
                return OpOutcome::Cancelled;
            }
            outcome = self.run_one(run);
            if outcome != OpOutcome::Succeeded {
                break;
            }
        }
        outcome
    }

    /// P9: hardened two-stage PR/MR checkout enforcing Laws 9, 11, 12, and 13:
    /// 1. Fetch into a unique operation-owned ref (`refs/invoke-git/pr-checkout/...`)
    ///    with `--no-tags --no-prune --no-write-fetch-head --refmap=` so configured
    ///    `remote.<name>.fetch` mappings can never move any other branch (Law 9)
    ///    and `FETCH_HEAD` is never touched.
    /// 2. Re-verify `index.lock`, `HEAD`, `index`, expected destination ref
    ///    absence, AND worktree cleanliness right before switching.
    /// 3. Resolve the temp ref to its exact commit OID and run
    ///    `git switch --no-track --no-overwrite-ignore -c <local_branch> <oid>`
    ///    with deferred cancellation so a slow `post-checkout` hook never
    ///    reports `Cancelled` after Git has already switched branches (Law 13).
    /// 4. Always delete the temp ref via RAII drop guard.
    fn run_checkout_pr(&self, remote: &str, remote_ref: &str, local_branch: &str) -> OpOutcome {
        static TEMP_REF_SEQ: AtomicU64 = AtomicU64::new(1);
        let seq = TEMP_REF_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp_ref = format!(
            "refs/invoke-git/pr-checkout/op-{}-{seq}",
            std::process::id()
        );
        struct TempRefGuard {
            git: Git,
            workdir: PathBuf,
            env: Vec<(OsString, OsString)>,
            temp_ref: String,
        }
        impl Drop for TempRefGuard {
            fn drop(&mut self) {
                let mut cmd = self.git.command(&self.workdir);
                cmd.args(crate::git::GLOBAL_ARGS)
                    .args(["update-ref", "-d", &self.temp_ref])
                    .envs(self.env.iter().map(|(k, v)| (k, v)))
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let _ = cmd.status();
            }
        }
        let _cleanup = TempRefGuard {
            git: self.git.clone(),
            workdir: self.paths.workdir.clone(),
            env: self.env.clone(),
            temp_ref: temp_ref.clone(),
        };

        // Stage 1: cancellable fetch into the isolated operation-owned temp ref.
        let mut fetch_argv: Vec<OsString> =
            crate::git::GLOBAL_ARGS.iter().map(OsString::from).collect();
        fetch_argv.extend(
            [
                "fetch",
                "--progress",
                "--no-tags",
                "--no-prune",
                "--no-write-fetch-head",
                "--refmap=",
                "--",
            ]
            .map(OsString::from),
        );
        fetch_argv.push(remote.into());
        fetch_argv.push(format!("+{remote_ref}:{temp_ref}").into());
        let fetch_outcome = self.run_one(Invocation {
            args: fetch_argv,
            stdin: None,
        });
        if fetch_outcome != OpOutcome::Succeeded {
            return fetch_outcome;
        }
        if self.cancelled() {
            return OpOutcome::Cancelled;
        }

        // Resolve our isolated temp ref to its exact commit OID and verify
        // worktree cleanliness.
        match crate::read::read_state(&self.paths) {
            Ok(disk) => {
                if disk.phase != invoke_core::Phase::Clean || !disk.status.entries.is_empty() {
                    return failed(
                        ErrorClass::PreconditionChanged,
                        "Working tree or repository state changed during fetch; commit, stash, or discard local changes and try again.".into(),
                    );
                }
            }
            Err(failure) => return OpOutcome::Failed(failure),
        }
        let target_oid = match precondition::open(&self.paths).and_then(|repo| {
            let mut reference = repo
                .find_reference(&temp_ref)
                .map_err(|e| format!("read fetched PR ref: {e}"))?;
            let id = reference
                .peel_to_id()
                .map_err(|e| format!("resolve fetched PR ref: {e}"))?;
            Ok(id.to_string())
        }) {
            Ok(oid) => oid,
            Err(err) => return failed(ErrorClass::Bug, err),
        };

        // Immediately before starting the deferred-cancellation switch, re-check
        // cancellation, `index.lock`, `HEAD`, `index`, and destination ref absence.
        if self.cancelled() {
            return OpOutcome::Cancelled;
        }
        if let Err(failure) = precondition::check(&self.paths, &self.expect) {
            return OpOutcome::Failed(failure);
        }
        if let Err(failure) = precondition::check_refs(&self.paths, &self.args.expected_refs()) {
            return OpOutcome::Failed(failure);
        }

        // Stage 2: local switch to the exact OID with `--no-overwrite-ignore`
        // and deferred cancellation (Law 13).
        let mut switch_argv: Vec<OsString> =
            crate::git::GLOBAL_ARGS.iter().map(OsString::from).collect();
        switch_argv
            .extend(["switch", "--no-track", "--no-overwrite-ignore", "-c"].map(OsString::from));
        switch_argv.push(local_branch.into());
        switch_argv.push(target_oid.into());
        let mut command = self.git.command(&self.paths.workdir);
        command
            .args(&switch_argv)
            .envs(self.env.iter().map(|(k, v)| (k, v)));
        let non_cancel = Control {
            cancelled: AtomicBool::new(false),
            defer_cancellation: true,
            askpass: Mutex::new(None),
        };
        match run_child(command, None, None, &non_cancel) {
            Ok(done) => classify(done.success, &done.output),
            Err(error) => failed(ErrorClass::Environment, format!("start git: {error}")),
        }
    }

    fn run_one(&self, run: Invocation) -> OpOutcome {
        let mut command = self.git.command(&self.paths.workdir);
        command
            .args(&run.args)
            .envs(self.env.iter().map(|(key, value)| (key, value)));
        // Task 23: continuing makes commits from the prepared message and
        // must never wait for an editor nobody can see. Set last, so neither
        // the user's `GIT_EDITOR` nor the extra environment brings one back.
        if self.args.may_open_editor() {
            // Git treats ":" as a no-op itself. Rebase needs no shell or
            // external "true" executable even when accepting a squash message.
            let editor = if crate::rebase::is_sequencer(&self.args) {
                ":"
            } else {
                "true"
            };
            command
                .env("GIT_EDITOR", editor)
                .env("GIT_SEQUENCE_EDITOR", editor);
        }
        let _todo = if let GitArgs::InteractiveRebase(plan) = &self.args {
            let Some(program) = &self.rebase_editor else {
                return failed(
                    ErrorClass::Environment,
                    "rebase editor executable unavailable".into(),
                );
            };
            match crate::rebase::TodoFile::prepare(&mut command, program, plan) {
                Ok(todo) => Some(todo),
                Err(error) => {
                    return failed(
                        ErrorClass::Environment,
                        format!("prepare rebase editor: {error}"),
                    );
                }
            }
        } else {
            None
        };
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
        if crate::rebase::is_sequencer(&self.args)
            && let Err(failure) = precondition::check(&self.paths, &self.expect)
        {
            return OpOutcome::Failed(failure);
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

/// Runs the submodule disk check and immediately re-verifies the calling repo's
/// precondition (`index.lock`, `HEAD`, `index`) right before spawning `git submodule update`.
fn run_submodule_preflight(
    paths: &RepoPaths,
    expect: &Expect,
    path: Option<&invoke_core::RepoPath>,
    check_sub: impl FnOnce(&RepoPaths, Option<&invoke_core::RepoPath>) -> Result<(), Failure>,
) -> Result<(), Failure> {
    check_sub(paths, path)?;
    precondition::check(paths, expect)
}

pub(crate) fn failed(class: ErrorClass, details: String) -> OpOutcome {
    OpOutcome::Failed(Failure { class, details })
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submodule_preflight_rechecks_precondition_after_submodule_scan() {
        let dir = std::env::temp_dir().join(format!(
            "invoke-sub-preflight-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success());

        let paths = RepoPaths {
            workdir: dir.clone(),
            git_dir: dir.join(".git"),
        };
        let expect = precondition::current(&paths).unwrap();

        // Simulate `index.lock` appearing during the submodule disk scan.
        let lock_path = paths.git_dir.join("index.lock");
        let res = run_submodule_preflight(&paths, &expect, None, |_, _| {
            std::fs::write(&lock_path, b"lock").unwrap();
            Ok(())
        });
        let _ = std::fs::remove_file(&lock_path);
        let _ = std::fs::remove_dir_all(&dir);

        match res {
            Err(f) => assert_eq!(f.class, ErrorClass::PreconditionChanged),
            Ok(()) => panic!("expected PreconditionChanged when index.lock appeared during scan"),
        }
    }
}
