//! Events (inputs to `step`) and commands (outputs for the shell).

use std::collections::BTreeSet;

use crate::ids::{IndexId, Oid, OpId, ReadId, RefName};
use crate::ops::{AuthAnswer, AuthPrompt, Failure, GitArgs, OpOutcome, Patch, Progress, Selection};
use crate::state::{DiskState, Head};

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

/// Everything needed to undo one operation (task 12 replays these).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoPoint {
    pub op: OpId,
    pub label: String,
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
    SnapshotForUndo(UndoPoint),
}

impl Command {
    /// Commands that change `.git` and so must respect the lock (law 12).
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Command::ApplyPatchToIndex { .. }
                | Command::RunGit { .. }
                | Command::SnapshotForUndo(_)
        )
    }
}
