//! Events (inputs to `step`) and commands (outputs for the shell).

use std::collections::BTreeSet;

use crate::ids::{BranchName, IndexId, Oid, OpId, ReadId, RefName};
use crate::ops::{AuthAnswer, AuthPrompt, Failure, GitArgs, OpOutcome, Patch, Progress, Selection};
use crate::state::{DiskState, Head};
use crate::undo::UndoReplay;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    // From the UI.
    StageRequested(Selection),
    UnstageRequested(Selection),
    CommitRequested {
        message: String,
        amend: bool,
    },
    PushRequested {
        force_with_lease: bool,
    },
    OpCancelRequested(OpId),
    AuthAnswered(OpId, AuthAnswer),
    UndoRequested,
    RefreshRequested,
    NoticeDismissed,
    // From the shell.
    /// The watcher saw changes (already debounced and classified).
    DiskChanged(ChangeSet),
    /// A fresh read of `.git` and status, answering `Command::ReadState`.
    StateRead {
        read: ReadId,
        disk: DiskState,
    },
    ReadFailed {
        read: ReadId,
        failure: Failure,
    },
    OpProgress(OpId, Progress),
    AuthNeeded(OpId, AuthPrompt),
    OpFinished(OpId, OpOutcome),
    /// The shell saved the index tree for the point of `op`
    /// (`Command::SnapshotForUndo`). A failed snapshot is reported as
    /// `OpFinished(op, Failed(..))` and the write is not run.
    UndoSnapshotTaken {
        op: OpId,
        tree: Oid,
    },
}

/// Part of the repo a change touched. The watcher (task 6) maps paths to
/// areas; changes inside `objects/` never produce one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Area {
    Worktree,
    Index,
    Head,
    Refs,
    /// `MERGE_HEAD`, `rebase-merge/` and the other in-progress markers.
    InProgress,
}

pub const ALL_AREAS: [Area; 5] = [
    Area::Worktree,
    Area::Index,
    Area::Head,
    Area::Refs,
    Area::InProgress,
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSet(pub BTreeSet<Area>);

/// What a read must cover. The reply is always a whole [`DiskState`]; the
/// scope only tells the shell which parts actually need reading again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadScope(pub BTreeSet<Area>);

impl ReadScope {
    pub fn full() -> Self {
        Self(ALL_AREAS.into_iter().collect())
    }
}

/// What a write expects to find on disk; the shell refuses the write with a
/// `PreconditionChanged` failure when disk differs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expect {
    pub head: Head,
    pub index: IndexId,
}

/// One ref an operation may move, with the value it had before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefValue {
    pub name: RefName,
    /// `None` when the ref does not exist yet (an unborn branch).
    pub oid: Option<Oid>,
}

/// Which kind of `HEAD` the operation started from. Undo refuses when the
/// user has switched branches since, so it never rewrites another branch's
/// index. An unborn branch counts as its branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadTarget {
    Branch(BranchName),
    Detached,
}

impl HeadTarget {
    pub fn of(head: &Head) -> Self {
        match head {
            Head::Branch { name, .. } | Head::Unborn(name) => HeadTarget::Branch(name.clone()),
            Head::Detached(_) => HeadTarget::Detached,
        }
    }
}

/// Everything recorded before an operation runs, to undo it later (task 12).
/// Task 13 adds the discarded content here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoPoint {
    pub op: OpId,
    pub label: String,
    pub head: HeadTarget,
    pub refs: Vec<RefValue>,
    pub index: IndexId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    ReadState {
        read: ReadId,
        scope: ReadScope,
    },
    ApplyPatchToIndex {
        op: OpId,
        patch: Patch,
        expect_index: IndexId,
    },
    RunGit {
        op: OpId,
        args: GitArgs,
        expect: Expect,
    },
    CancelOp(OpId),
    AnswerAuth(OpId, AuthAnswer),
    /// Save the index tree and keep it alive under
    /// `refs/invoke-git/undo/<op>`; answered by `UndoSnapshotTaken`.
    SnapshotForUndo(UndoPoint),
    /// Put refs and the index back (an undo).
    ReplayUndo(UndoReplay),
    /// Delete the saved trees of points that left the stack. Touches only
    /// our own `refs/invoke-git/undo/*`, so it is not a write in the sense
    /// of law 12.
    PruneUndo(Vec<OpId>),
}

impl Command {
    /// Commands that change `.git` and so must respect the lock (law 12).
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Command::ApplyPatchToIndex { .. }
                | Command::RunGit { .. }
                | Command::SnapshotForUndo(_)
                | Command::ReplayUndo(_)
        )
    }
}
