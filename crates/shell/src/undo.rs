//! `SnapshotForUndo`, `ReplayUndo` and `PruneUndo`: the git half of undo
//! (`docs/spec/core-design.md` -> "Undo design"). Everything goes through
//! the git CLI; no decisions are made here (the core planned the replay
//! and carries every expected value).
//!
//! Saved index trees live under `refs/invoke-git/undo/<op>` so `git gc`
//! keeps them.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use invoke_core::{
    ErrorClass, Event, Failure, Oid, OpId, OpOutcome, Reversal, StashRestore, UndoPoint, UndoReplay,
};

use crate::git::{GLOBAL_ARGS, Git};
use crate::precondition::{self, RepoPaths};

/// Where the saved tree of the point of `op` is kept.
pub fn undo_ref(op: OpId) -> String {
    format!("refs/invoke-git/undo/{}", op.0)
}

/// Task 20: where the commit of a dropped stash is kept alive, so the drop
/// can be undone and `git gc` leaves it alone.
pub fn stash_keep_ref(op: OpId) -> String {
    format!("refs/invoke-git/undo/{}-stash", op.0)
}

/// The answer to `SnapshotForUndo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Snapshotted {
    /// Deliver this event and run the operation.
    Taken(Event),
    /// Deliver this event and do not run the operation.
    Failed(Event),
}

pub struct UndoRunner {
    git: Git,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
}

impl UndoRunner {
    pub fn new(git: Git, paths: RepoPaths) -> Self {
        Self {
            git,
            paths,
            env: Vec::new(),
        }
    }

    /// Extra environment for every git run (tests use it to isolate config).
    pub fn with_env(mut self, env: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        self.env.extend(env);
        self
    }

    /// Saves the index as a tree object and keeps it alive. `Taken` carries
    /// the `UndoSnapshotTaken` event. `Failed` carries the `OpFinished` for
    /// the operation: the driver must then not run the operation's `RunGit`.
    pub fn snapshot(&self, point: &UndoPoint) -> Snapshotted {
        match self.try_snapshot(point) {
            Ok(tree) => Snapshotted::Taken(Event::UndoSnapshotTaken { op: point.op, tree }),
            Err(failure) => {
                Snapshotted::Failed(Event::OpFinished(point.op, OpOutcome::Failed(failure)))
            }
        }
    }

    /// The tree is written from a private copy of the index file
    /// (`GIT_INDEX_FILE`): `git write-tree` on the real index rewrites it
    /// (cache-tree), which would change its id under the operation's
    /// expected value.
    fn try_snapshot(&self, point: &UndoPoint) -> Result<Oid, Failure> {
        let op = point.op;
        let fail = |class, details: String| Failure { class, details };
        let current =
            precondition::current(&self.paths).map_err(|details| fail(ErrorClass::Bug, details))?;
        if current.index != point.index {
            return Err(fail(
                ErrorClass::PreconditionChanged,
                format!(
                    "index changed before the undo snapshot: expected {:?}, found {:?}",
                    point.index, current.index
                ),
            ));
        }
        let real = self.paths.git_dir.join("index");
        let copy = self
            .paths
            .git_dir
            .join(format!("invoke-undo-index-{}", op.0));
        // An absent index is an empty one: write-tree gives the empty tree.
        if real.exists() {
            fs::copy(&real, &copy)
                .map_err(|error| fail(ErrorClass::Environment, format!("copy index: {error}")))?;
        }
        let written = self.run(
            &["write-tree"],
            &[("GIT_INDEX_FILE", copy.as_os_str())],
            None,
        );
        let _ = fs::remove_file(&copy);
        let tree = written.map_err(|details| fail(ErrorClass::Bug, details))?;
        let tree = Oid::from_hex(tree.trim())
            .map_err(|error| fail(ErrorClass::Bug, format!("write-tree: {error:?}")))?;
        self.run(&["update-ref", &undo_ref(op), tree.as_hex()], &[], None)
            .map_err(|details| fail(ErrorClass::Bug, details))?;
        // Task 20: a drop's undo needs the stash commit, which nothing but
        // the stash list keeps alive.
        if let Reversal::StashStore(entry) | Reversal::StashKept(entry) = &point.reverse {
            self.run(
                &["update-ref", &stash_keep_ref(op), entry.id.as_hex()],
                &[],
                None,
            )
            .map_err(|details| fail(ErrorClass::Bug, details))?;
        }
        Ok(tree)
    }

    /// Restores the recorded refs, atomically and only if each still holds
    /// the expected value, then the index from the saved tree. Returns the
    /// `OpFinished` for the undo.
    pub fn replay(&self, replay: &UndoReplay) -> Event {
        Event::OpFinished(replay.op, self.replay_outcome(replay))
    }

    fn replay_outcome(&self, replay: &UndoReplay) -> OpOutcome {
        if let Err(failure) = precondition::check(&self.paths, &replay.expect) {
            return OpOutcome::Failed(failure);
        }
        if let Some(restore) = &replay.stash {
            return self.restore_stash(restore);
        }
        let zero = "0".repeat(replay.tree.as_hex().len());
        let transaction = |restore: bool| {
            let mut script = String::from("start\n");
            for item in &replay.refs {
                let (from, to) = if restore {
                    (&item.expect, &item.restore)
                } else {
                    (&item.restore, &item.expect)
                };
                let hex = |oid: &Option<Oid>| oid.as_ref().map_or(zero.clone(), |o| o.to_string());
                match to {
                    Some(to) => {
                        script.push_str(&format!("update {} {to} {}\n", item.name.0, hex(from)));
                    }
                    None if from.is_some() => {
                        script.push_str(&format!("delete {} {}\n", item.name.0, hex(from)));
                    }
                    None => {}
                }
            }
            script.push_str("prepare\ncommit\n");
            script
        };
        let refs = |restore: bool| {
            self.run(
                &["update-ref", "--no-deref", "--stdin"],
                &[],
                Some(transaction(restore).as_bytes()),
            )
        };
        if let Err(details) = refs(true) {
            // Nothing was changed: the transaction is all or nothing.
            return failed(ErrorClass::PreconditionChanged, details);
        }
        if let Err(details) = self.run(&["read-tree", replay.tree.as_hex()], &[], None) {
            let rollback = match refs(false) {
                Ok(_) => "refs rolled back".to_owned(),
                Err(error) => format!("refs could NOT be rolled back: {error}"),
            };
            return failed(ErrorClass::Bug, format!("{details}\n{rollback}"));
        }
        OpOutcome::Succeeded
    }

    /// Undo of a stash drop: put the stash back on top of the list, if the
    /// list is still as the drop left it. Index and refs are not touched
    /// (`git stash store` writes only `refs/stash` and its reflog).
    fn restore_stash(&self, restore: &StashRestore) -> OpOutcome {
        let fail = |class, details: String| failed(class, details);
        let repo = match precondition::open(&self.paths) {
            Ok(repo) => repo,
            Err(details) => return fail(ErrorClass::Bug, details),
        };
        match crate::read::read_stashes(&repo) {
            Ok(stashes) if invoke_core::stash::top(&stashes) == restore.expect_top.as_ref() => {}
            Ok(_) => {
                return fail(
                    ErrorClass::PreconditionChanged,
                    "the stash list changed since the drop".into(),
                );
            }
            Err(details) => return fail(ErrorClass::Bug, details),
        }
        let message = format!("--message={}", restore.entry.message);
        match self.run(
            &["stash", "store", &message, restore.entry.id.as_hex()],
            &[],
            None,
        ) {
            Ok(_) => OpOutcome::Succeeded,
            Err(details) => fail(ErrorClass::Bug, details),
        }
    }

    /// Deletes the saved trees of points that left the stack. Best effort:
    /// a ref that is already gone is fine.
    pub fn prune(&self, ops: &[OpId]) {
        for op in ops {
            let _ = self.run(&["update-ref", "-d", &undo_ref(*op)], &[], None);
            let _ = self.run(&["update-ref", "-d", &stash_keep_ref(*op)], &[], None);
        }
    }

    /// Runs git and returns stdout, or stderr (with the command) on failure.
    fn run(
        &self,
        args: &[&str],
        extra_env: &[(&str, &std::ffi::OsStr)],
        stdin: Option<&[u8]>,
    ) -> Result<String, String> {
        let mut command: Command = self.git.command(&self.paths.workdir);
        command
            .args(GLOBAL_ARGS)
            .args(args)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .envs(extra_env.iter().copied())
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("start git: {error}"))?;
        if let (Some(mut pipe), Some(bytes)) = (child.stdin.take(), stdin) {
            // Small scripts only: the pipe buffer takes them whole.
            let _ = pipe.write_all(bytes);
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("wait for git: {error}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

fn failed(class: ErrorClass, details: String) -> OpOutcome {
    OpOutcome::Failed(Failure { class, details })
}
