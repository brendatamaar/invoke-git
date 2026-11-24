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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use invoke_core::{
    ErrorClass, Event, Failure, Head, HeadRestore, HeadTarget, IndexEffect, Oid, OpId, OpOutcome,
    Reversal, StashRestore, UndoPoint, UndoReplay,
};

use crate::classify::classify;
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

/// Task 13: where the content of a discarded file, as it was before the
/// discard, is kept alive (a tree holding the file's blob and mode).
pub fn discard_ref(op: OpId) -> String {
    format!("refs/invoke-git/undo/{}-discard", op.0)
}

/// Task 13: the same for the file as the discard left it. An undo only
/// writes the saved content back while the file still holds this.
pub fn discard_after_ref(op: OpId) -> String {
    format!("refs/invoke-git/undo/{}-discard-after", op.0)
}

pub struct UndoRunner {
    pub(crate) git: Git,
    pub(crate) paths: RepoPaths,
    pub(crate) env: Vec<(OsString, OsString)>,
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
        if self.paths.git_dir.join("index.lock").exists() {
            return Err(fail(
                ErrorClass::PreconditionChanged,
                "index.lock is held".into(),
            ));
        }
        if HeadTarget::of(&current.head) != point.head {
            return Err(fail(
                ErrorClass::PreconditionChanged,
                "HEAD moved before the undo snapshot".into(),
            ));
        }
        precondition::check_refs(&self.paths, &point.refs)?;
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
        if point.index_effect == IndexEffect::RestoreBytes {
            let _lock = precondition::IndexLock::acquire(&self.paths, &current)?;
            return self
                .save_index_bytes(op, &real)
                .map_err(|details| fail(ErrorClass::Bug, details));
        }
        let discard_lock = matches!(point.reverse, Reversal::Discard { .. })
            .then(|| precondition::IndexLock::acquire(&self.paths, &current))
            .transpose()?;
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
        if let Some(lock) = &discard_lock {
            lock.verify(&self.paths, &current)?;
        } else if self.paths.git_dir.join("index.lock").exists() {
            return Err(fail(
                ErrorClass::PreconditionChanged,
                "index.lock is held".into(),
            ));
        }
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
        // Task 13: a discard's undo needs the file as it is right now, and
        // the snapshot is the last moment before the patch runs.
        if let Reversal::Discard { path } = &point.reverse {
            self.save_file(&discard_ref(op), path)?;
        }
        Ok(tree)
    }

    fn save_index_bytes(&self, op: OpId, real: &std::path::Path) -> Result<Oid, String> {
        let bytes = match fs::read(real) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(format!("read index: {error}")),
        };
        let hash_blob = |bytes: &[u8]| {
            self.run(
                &["hash-object", "-w", "--no-filters", "--stdin"],
                &[],
                Some(bytes),
            )
        };
        let index = hash_blob(&bytes)?;
        let mut entries = format!("100644 blob {}\tindex\n", index.trim());
        if !bytes.is_empty() {
            let modified = real
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map_err(|error| format!("read index timestamp: {error}"))?;
            let (duration, sign) = match modified.duration_since(UNIX_EPOCH) {
                Ok(duration) => (duration, 1),
                Err(error) => (error.duration(), -1),
            };
            let timestamp = (sign * duration.as_nanos() as i128).to_le_bytes();
            let blob = hash_blob(&timestamp)?;
            entries.push_str(&format!("100644 blob {}\tindex-time\n", blob.trim()));
        }
        let repo = precondition::open(&self.paths)?;
        if let Some(name) = shared_index_name(&bytes, repo.object_hash())? {
            let shared = fs::read(self.paths.git_dir.join(&name))
                .map_err(|error| format!("read {name}: {error}"))?;
            let blob = hash_blob(&shared)?;
            entries.push_str(&format!("100644 blob {}\t{name}\n", blob.trim()));
        }
        // Git's GC cannot see the staged object ids inside a raw index blob.
        // Keep the index tree reachable alongside it.
        let scratch = precondition::ScratchDir::create(&self.paths.git_dir)
            .map_err(|error| format!("prepare index snapshot: {error}"))?;
        let copy = scratch.0.join("index");
        if !bytes.is_empty() {
            fs::write(&copy, &bytes).map_err(|error| format!("copy index: {error}"))?;
        }
        let staged = self.run(
            &["write-tree"],
            &[("GIT_INDEX_FILE", copy.as_os_str())],
            None,
        )?;
        entries.push_str(&format!("040000 tree {}\tstaged\n", staged.trim()));
        let tree = self.run(&["mktree"], &[], Some(entries.as_bytes()))?;
        let id = Oid::from_hex(tree.trim()).map_err(|error| format!("save index: {error:?}"))?;
        self.run(&["update-ref", &undo_ref(op), id.as_hex()], &[], None)?;
        Ok(id)
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
        // Task 13: a discard's undo writes one file back and touches no ref
        // and not the index.
        if let Some(restore) = &replay.discard {
            let _lock = match precondition::IndexLock::acquire(&self.paths, &replay.expect) {
                Ok(lock) => lock,
                Err(failure) => return OpOutcome::Failed(failure),
            };
            return self.restore_discard(restore);
        }
        if replay.index_effect == IndexEffect::RestoreBytes {
            return self.restore_index_bytes(replay);
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
        if let Some(restore) = &replay.head {
            // Check every ref first, so a stale one is found before `HEAD`
            // moves (a checkout cannot be undone without side effects).
            let mut script = String::from("start\n");
            for item in &replay.refs {
                let expect = item.expect.as_ref().map_or(zero.clone(), |o| o.to_string());
                script.push_str(&format!("verify {} {expect}\n", item.name.0));
            }
            script.push_str("prepare\nabort\n");
            if let Err(details) = self.run(
                &["update-ref", "--no-deref", "--stdin"],
                &[],
                Some(script.as_bytes()),
            ) {
                return failed(ErrorClass::PreconditionChanged, details);
            }
            if let Err(details) = self.move_head(restore) {
                // git refuses a checkout before changing anything.
                return classify(false, &details);
            }
        }
        if let Err(details) = refs(true) {
            // The ref transaction is all or nothing; only `HEAD` may have
            // moved, and that is put back.
            let back = replay.head.as_ref().map_or(String::new(), |restore| {
                match self.rollback_head(restore, &replay.expect.head) {
                    Ok(()) => "\nHEAD put back".to_owned(),
                    Err(error) => format!("\nHEAD could NOT be put back: {error}"),
                }
            });
            return failed(ErrorClass::PreconditionChanged, format!("{details}{back}"));
        }
        if replay.index_effect == IndexEffect::Rewrites
            && let Err(details) = self.run(&["read-tree", replay.tree.as_hex()], &[], None)
        {
            let rollback = match refs(false) {
                Ok(_) => "refs rolled back".to_owned(),
                Err(error) => format!("refs could NOT be rolled back: {error}"),
            };
            return failed(ErrorClass::Bug, format!("{details}\n{rollback}"));
        }
        OpOutcome::Succeeded
    }

    fn restore_index_bytes(&self, replay: &UndoReplay) -> OpOutcome {
        let lock = match precondition::IndexLock::acquire(&self.paths, &replay.expect) {
            Ok(lock) => lock,
            Err(failure) => return OpOutcome::Failed(failure),
        };
        let result = (|| {
            let repo = precondition::open(&self.paths)?;
            let id = gix::ObjectId::from_hex(replay.tree.as_hex().as_bytes())
                .map_err(|error| error.to_string())?;
            let tree = repo.find_tree(id).map_err(|error| error.to_string())?;
            let read = |name: &str| -> Result<gix::Blob<'_>, String> {
                let entry = tree
                    .lookup_entry_by_path(name)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| format!("index snapshot is missing {name}"))?;
                repo.find_blob(entry.object_id())
                    .map_err(|error| error.to_string())
            };
            let index = read("index")?;
            let modified = if index.data.is_empty() {
                None
            } else {
                Some(index_timestamp(&read("index-time")?.data)?)
            };
            if let Some(name) = shared_index_name(&index.data, repo.object_hash())? {
                let base = read(&name)?;
                let path = self.paths.git_dir.join(&name);
                match fs::read(&path) {
                    Ok(bytes) if bytes == base.data => {}
                    Ok(_) => return Err(format!("{name} differs from the saved shared index")),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        let scratch = precondition::ScratchDir::create(&self.paths.git_dir)
                            .map_err(|error| format!("prepare shared index restore: {error}"))?;
                        let copy = scratch.0.join(&name);
                        fs::write(&copy, &base.data)
                            .and_then(|()| fs::rename(&copy, &path))
                            .map_err(|error| format!("restore {name}: {error}"))?;
                    }
                    Err(error) => return Err(format!("read {name}: {error}")),
                }
            }
            Ok((index.data.clone(), modified))
        })();
        match result {
            Ok((bytes, modified)) => match lock.replace(&self.paths.git_dir, &bytes, modified) {
                Ok(()) => OpOutcome::Succeeded,
                Err(error) => failed(ErrorClass::Environment, format!("restore index: {error}")),
            },
            Err(details) => failed(ErrorClass::Bug, details),
        }
    }

    /// Puts `HEAD` where the operation found it.
    fn move_head(&self, restore: &HeadRestore) -> Result<(), String> {
        match restore {
            HeadRestore::Symbolic(name) => {
                self.run(&["symbolic-ref", "HEAD", &name.to_ref().0], &[], None)
            }
            HeadRestore::Branch(name) => self.run(&["switch", "--no-guess", &name.0], &[], None),
            HeadRestore::Detached(oid) => {
                self.run(&["switch", "--detach", oid.as_hex()], &[], None)
            }
        }
        .map(|_| ())
    }

    /// Undoes `move_head`: back to `now`, as read before the replay.
    fn rollback_head(&self, restore: &HeadRestore, now: &Head) -> Result<(), String> {
        let result = match (restore, now) {
            (HeadRestore::Symbolic(_), Head::Branch { name, .. } | Head::Unborn(name)) => {
                self.run(&["symbolic-ref", "HEAD", &name.to_ref().0], &[], None)
            }
            (_, Head::Branch { name, .. } | Head::Unborn(name)) => {
                self.run(&["switch", "--no-guess", &name.0], &[], None)
            }
            (_, Head::Detached(oid)) => self.run(&["switch", "--detach", oid.as_hex()], &[], None),
        };
        result.map(|_| ())
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
            if self.paths.git_dir.join("index.lock").exists() {
                return;
            }
            let _ = self.run(&["update-ref", "-d", &undo_ref(*op)], &[], None);
            let _ = self.run(&["update-ref", "-d", &stash_keep_ref(*op)], &[], None);
            let _ = self.run(&["update-ref", "-d", &discard_ref(*op)], &[], None);
            let _ = self.run(&["update-ref", "-d", &discard_after_ref(*op)], &[], None);
        }
    }

    /// Runs git and returns stdout, or stderr (with the command) on failure.
    pub(crate) fn run(
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
        // stdin is written on its own thread while `wait_with_output` drains
        // stdout and stderr: git may write a lot (`GIT_TRACE`, a large
        // index's errors) before it reads all of stdin, and a full pipe on
        // each side would block both forever. Dropping the pipe at the end
        // closes git's stdin.
        let output = std::thread::scope(|scope| {
            if let (Some(mut pipe), Some(bytes)) = (child.stdin.take(), stdin) {
                scope.spawn(move || {
                    let _ = pipe.write_all(bytes);
                });
            }
            child.wait_with_output()
        })
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

fn shared_index_name(bytes: &[u8], hash: gix::hash::Kind) -> Result<Option<String>, String> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let timestamp = gix::index::State::new(hash).timestamp();
    let (index, _) = gix::index::State::from_bytes(bytes, timestamp, hash, Default::default())
        .map_err(|error| format!("decode saved index: {error}"))?;
    Ok(index
        .link()
        .filter(|link| !link.shared_index_checksum.is_null())
        .map(|link| format!("sharedindex.{}", link.shared_index_checksum)))
}

fn index_timestamp(bytes: &[u8]) -> Result<SystemTime, String> {
    let nanos = i128::from_le_bytes(bytes.try_into().map_err(|_| "invalid index timestamp")?);
    let distance = nanos.unsigned_abs();
    let seconds =
        u64::try_from(distance / 1_000_000_000).map_err(|_| "index timestamp overflow")?;
    let duration = Duration::new(seconds, (distance % 1_000_000_000) as u32);
    let time = if nanos < 0 {
        UNIX_EPOCH.checked_sub(duration)
    } else {
        UNIX_EPOCH.checked_add(duration)
    };
    time.ok_or_else(|| "index timestamp outside the supported range".into())
}

fn failed(class: ErrorClass, details: String) -> OpOutcome {
    OpOutcome::Failed(Failure { class, details })
}

/// What is left of the undo points kept from an earlier run (task 22).
#[derive(Debug, PartialEq, Eq)]
pub struct Reconciled {
    /// The points whose saved trees (and stash or discard copies) are still
    /// in the repository, in their old order.
    pub kept: Vec<invoke_core::UndoEntry>,
    /// Ops that have refs under `refs/invoke-git/undo/` but no kept point:
    /// to be pruned.
    pub orphans: Vec<OpId>,
}

/// The op a ref under `refs/invoke-git/undo/` belongs to: `12`, `12-stash`,
/// `12-discard` and `12-discard-after` are all op 12.
pub fn op_of_ref(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("refs/invoke-git/undo/")?;
    let digits = rest.split('-').next()?;
    let op: u64 = digits.parse().ok()?;
    // Only the spellings this app writes: no sign, no leading zero.
    (op.to_string() == digits).then_some(op)
}

/// Checks the points kept from an earlier run against the refs now in the
/// repository: a point whose saved tree is gone cannot be undone and is left
/// out; refs of ops that are not kept (a point that fell off the stack, a
/// crash between a snapshot and its record) are reported for pruning.
pub fn reconcile(
    paths: &RepoPaths,
    entries: Vec<invoke_core::UndoEntry>,
) -> Result<Reconciled, String> {
    let repo = precondition::open(paths)?;
    let platform = repo
        .references()
        .map_err(|error| format!("read refs: {error}"))?;
    let found = platform
        .prefixed("refs/invoke-git/undo/")
        .map_err(|error| format!("read refs: {error}"))?;
    let mut names = std::collections::BTreeSet::new();
    for reference in found {
        let reference = reference.map_err(|error| format!("read ref: {error}"))?;
        names.insert(reference.name().as_bstr().to_string());
    }
    let has = |name: String| names.contains(&name);
    let kept: Vec<invoke_core::UndoEntry> = entries
        .into_iter()
        .filter(|entry| {
            let op = entry.point.op;
            has(undo_ref(op))
                && match &entry.point.reverse {
                    Reversal::StashStore(_) | Reversal::StashKept(_) => has(stash_keep_ref(op)),
                    Reversal::Discard { .. } => has(discard_ref(op)) && has(discard_after_ref(op)),
                    Reversal::Refs | Reversal::Never(_) => true,
                }
        })
        .collect();
    let live: std::collections::BTreeSet<u64> = kept.iter().map(|entry| entry.point.op.0).collect();
    let mut orphans: Vec<OpId> = names
        .iter()
        .filter_map(|name| op_of_ref(name))
        .filter(|op| !live.contains(op))
        .collect::<std::collections::BTreeSet<u64>>()
        .into_iter()
        .map(OpId)
        .collect();
    orphans.sort();
    Ok(Reconciled { kept, orphans })
}

#[cfg(test)]
mod reconcile_tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn a_ref_belongs_to_the_op_in_its_name() {
        for (name, op) in [
            ("refs/invoke-git/undo/12", Some(12)),
            ("refs/invoke-git/undo/12-stash", Some(12)),
            ("refs/invoke-git/undo/12-discard-after", Some(12)),
            ("refs/invoke-git/undo/0", Some(0)),
            ("refs/invoke-git/undo/012", None),
            ("refs/invoke-git/undo/x", None),
            ("refs/invoke-git/undo/", None),
            ("refs/heads/12", None),
            ("refs/invoke-git/undo/-5", None),
        ] {
            assert_eq!(op_of_ref(name), op, "{name}");
        }
    }

    proptest! {
        #[test]
        fn the_op_of_a_ref_we_write_is_found(op in any::<u64>()) {
            for name in [
                undo_ref(OpId(op)),
                stash_keep_ref(OpId(op)),
                discard_ref(OpId(op)),
                discard_after_ref(OpId(op)),
            ] {
                prop_assert_eq!(op_of_ref(&name), Some(op));
            }
        }
    }
}

#[cfg(test)]
mod run_tests {
    use super::*;

    /// Cross review: git may write a lot to stderr (`GIT_TRACE`, hooks)
    /// before it reads stdin. Writing all of stdin first would then block
    /// both sides forever.
    #[test]
    fn big_stdin_and_chatty_stderr_do_not_deadlock() {
        let dir = std::env::temp_dir().join(format!(
            "invoke-git-undo-run-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let global = dir.join("gitconfig");
        fs::write(&global, "").unwrap();
        let runner = UndoRunner::new(
            Git::detect("git").unwrap(),
            RepoPaths {
                git_dir: dir.join(".git"),
                workdir: dir.clone(),
            },
        )
        .with_env([
            (OsString::from("GIT_CONFIG_NOSYSTEM"), OsString::from("1")),
            (OsString::from("GIT_CONFIG_GLOBAL"), global.into_os_string()),
        ]);
        let size = 4 << 20;
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let alias = "!i=0; while [ $i -lt 4000 ]; do \
                 echo 0123456789012345678901234567890123456789012345678901234567890 >&2; \
                 i=$((i+1)); done; wc -c";
            let env: [(&str, &std::ffi::OsStr); 3] = [
                ("GIT_CONFIG_COUNT", "1".as_ref()),
                ("GIT_CONFIG_KEY_0", "alias.noisy".as_ref()),
                ("GIT_CONFIG_VALUE_0", alias.as_ref()),
            ];
            let _ = send.send(runner.run(&["noisy"], &env, Some(&vec![b'x'; size])));
        });
        let result = receive
            .recv_timeout(Duration::from_secs(60))
            .expect("the runner and git deadlocked");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(result.unwrap().trim(), size.to_string());
    }
}
