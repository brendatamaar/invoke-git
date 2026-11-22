//! Whether a commit request can run on the snapshot it is dispatched on,
//! and which kind of commit it then is (task 14). Pure.

use crate::state::{CommitRefusal, DiskState, Head, Phase, Refusal};

/// What a `git commit` run will be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitKind {
    /// A new commit from what is staged.
    Commit,
    /// Replace the tip: same author and parents, new message, and whatever
    /// is staged on top (nothing staged is fine, that is a reword).
    Amend,
    /// A merge is in progress with every conflict resolved: the commit
    /// is the merge commit, with `MERGE_HEAD` as its second parent. It is
    /// allowed with nothing new staged, since a merge may end up equal to
    /// its first parent's tree.
    ConcludeMerge,
}

impl CommitKind {
    /// The first word of the undo label: `Commit 'Fix login'`.
    pub fn verb(self) -> &'static str {
        match self {
            CommitKind::Commit => "Commit",
            CommitKind::Amend => "Amend",
            CommitKind::ConcludeMerge => "Conclude merge",
        }
    }
}

/// Decides a commit against the fresh snapshot, or says why not. The
/// message was checked when the request arrived; this is what depends on
/// the disk.
pub fn plan(disk: &DiskState, amend: bool) -> Result<CommitKind, Refusal> {
    match &disk.phase {
        Phase::Clean if amend => match disk.head {
            Head::Unborn(_) => Err(Refusal::Commit(CommitRefusal::AmendUnborn)),
            Head::Branch { .. } | Head::Detached(_) => Ok(CommitKind::Amend),
        },
        Phase::Clean => {
            if disk.status.staged().next().is_none() {
                Err(Refusal::Commit(CommitRefusal::NothingStaged))
            } else {
                Ok(CommitKind::Commit)
            }
        }
        Phase::Merging { .. } if amend => Err(Refusal::Commit(CommitRefusal::AmendDuringMerge)),
        Phase::Merging { conflicts } if !conflicts.paths.is_empty() => {
            Err(Refusal::Commit(CommitRefusal::UnresolvedConflicts))
        }
        Phase::Merging { .. } => Ok(CommitKind::ConcludeMerge),
        // Their own `--continue` finishes these (task 23).
        Phase::Rebasing { .. } | Phase::CherryPicking { .. } | Phase::Reverting { .. } => {
            Err(Refusal::RepoBusy)
        }
    }
}
