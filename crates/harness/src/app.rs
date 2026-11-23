//! The side under test: the real core's `step()` driving the real shell
//! (git runner, staging runner, undo runner, state read) on one repo, one
//! command at a time. No UI, no watcher thread: an outside change reaches
//! the core as the `DiskChanged` the watcher would have sent.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use invoke_core::event::ALL_AREAS;
use invoke_core::{
    Area, ChangeSet, Command, Event, Failure, Notice, OpId, OpOutcome, Refusal, RepoId, RepoState,
    open, step,
};
use shell::undo::Snapshotted;
use shell::{DiscardRunner, Git, GitRunner, StagingRunner, UndoRunner, read::read_event};

use crate::gitdir::RepoDir;

/// What the core said about one request, read off its notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    /// Turned down before anything ran, with a reason.
    Refused(Refusal),
    /// Ran and ended badly; the class is the shell's classification.
    Failed(Failure),
    Cancelled,
    /// Neither a refusal nor an end: the request was silently dropped (or
    /// is still waiting in the queue).
    Nothing,
}

pub struct App {
    pub repo: RepoDir,
    pub state: RepoState,
    runner: GitRunner,
    staging: StagingRunner,
    discard: DiscardRunner,
    undo: UndoRunner,
    /// Operations whose undo snapshot failed: their git run is skipped.
    skip: BTreeSet<OpId>,
}

impl App {
    pub fn open(repo: RepoDir, editor: Option<&Path>) -> Self {
        let git = || Git::detect("git").expect("git on PATH");
        let (state, commands) = open(RepoId {
            root_commit: None,
            remote_url: None,
        });
        let mut runner = GitRunner::new(git(), repo.paths.clone()).with_env(repo.env.clone());
        if let Some(editor) = editor {
            runner = runner.with_rebase_editor(editor);
        }
        let mut app = Self {
            runner,
            staging: StagingRunner::new(git(), repo.paths.clone()).with_env(repo.env.clone()),
            discard: DiscardRunner::new(git(), repo.paths.clone()).with_env(repo.env.clone()),
            undo: UndoRunner::new(git(), repo.paths.clone()).with_env(repo.env.clone()),
            repo,
            state,
            skip: BTreeSet::new(),
        };
        for command in commands {
            app.exec(command);
        }
        app
    }

    pub fn deliver(&mut self, event: Event) {
        let (state, commands) = step(self.state.clone(), event);
        self.state = state;
        for command in commands {
            self.exec(command);
        }
    }

    fn exec(&mut self, command: Command) {
        match command {
            Command::ReadState { read, .. } => self.deliver(read_event(&self.repo.paths, read)),
            Command::SnapshotForUndo(point) => match self.undo.snapshot(&point) {
                Snapshotted::Taken(event) => self.deliver(event),
                Snapshotted::Failed(event) => {
                    self.skip.insert(point.op);
                    self.deliver(event);
                }
            },
            Command::RunGit { op, args, expect } => {
                if self.skip.remove(&op) {
                    return;
                }
                let (sender, events) = mpsc::channel();
                self.runner.run(op, args, expect, sender);
                loop {
                    let event = events
                        .recv_timeout(Duration::from_secs(60))
                        .expect("git operation never finished");
                    let done = matches!(event, Event::OpFinished(..));
                    self.deliver(event);
                    if done {
                        break;
                    }
                }
            }
            Command::ApplyPatchToIndex {
                op,
                patch,
                expect_index,
            } => self.deliver(self.staging.apply(op, &patch, &expect_index)),
            Command::ApplyPatchToWorktree {
                op,
                patch,
                path,
                expect_index,
                expect_file,
            } => {
                if self.skip.remove(&op) {
                    return;
                }
                let done =
                    self.discard
                        .apply(op, &patch, &path, &expect_index, expect_file.as_ref());
                self.deliver(done);
            }
            Command::ReplayUndo(replay) => self.deliver(self.undo.replay(&replay)),
            Command::PruneUndo(ops) => self.undo.prune(&ops),
            other => panic!("the harness does not drive {other:?}"),
        }
    }

    /// What the watcher sends after something changed behind the app's back.
    pub fn disk_changed(&mut self, areas: &[Area]) {
        self.deliver(Event::DiskChanged(ChangeSet(
            areas.iter().copied().collect(),
        )));
    }

    pub fn everything_changed(&mut self) {
        self.deliver(Event::DiskChanged(ChangeSet(
            ALL_AREAS.into_iter().collect(),
        )));
    }

    /// Sends a request with a clean notice and reports how it ended.
    pub fn request(&mut self, event: Event) -> Outcome {
        self.deliver(Event::NoticeDismissed);
        self.deliver(event);
        self.outcome()
    }

    pub fn outcome(&self) -> Outcome {
        match &self.state.notice {
            Some(Notice::Refused { reason, .. }) => Outcome::Refused(reason.clone()),
            Some(Notice::OpEnded { outcome, .. }) => match outcome {
                OpOutcome::Succeeded => Outcome::Succeeded,
                OpOutcome::Failed(failure) => Outcome::Failed(failure.clone()),
                OpOutcome::Cancelled => Outcome::Cancelled,
            },
            Some(Notice::ReadFailed { failure, .. } | Notice::PushRejected { failure, .. }) => {
                Outcome::Failed(failure.clone())
            }
            None => Outcome::Nothing,
        }
    }

    /// Nothing running or waiting, and the newest read has landed.
    pub fn is_idle(&self) -> bool {
        self.state.ops.running.is_none()
            && self.state.ops.queued.is_empty()
            && self.state.fresh_disk().is_some()
    }
}
