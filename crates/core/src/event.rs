//! Events (inputs to `step`) and commands (outputs for the shell).

use std::collections::BTreeSet;

use crate::conflict::ResolveChoice;
use crate::ids::{BranchName, IndexId, Oid, OpId, ReadId, RefName, RepoPath, TagName};
use crate::ops::{
    AuthAnswer, AuthPrompt, BranchStart, DiscardRequest, Failure, GitArgs, OpOutcome, Patch,
    Progress, PullMode, RemoteBranch, StageRequest,
};
use crate::reflog::ResetMode;
use crate::state::{DiskState, Head};
use crate::undo::{Reversal, UndoEntry, UndoReplay};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    // From the UI.
    StageRequested(StageRequest),
    UnstageRequested(StageRequest),
    // Task 13.
    DiscardRequested(DiscardRequest),
    CommitRequested {
        message: String,
        amend: bool,
    },
    /// Push the checked-out branch to `to`. With `force_with_lease` the
    /// lease is the remote-tracking value in the snapshot at this moment,
    /// the one the user reviewed; no known value, no force (law 10).
    PushRequested {
        to: RemoteBranch,
        force_with_lease: bool,
        set_upstream: bool,
    },
    // Task 17.
    FetchRequested {
        remote: String,
    },
    PullRequested {
        from: RemoteBranch,
        mode: PullMode,
    },
    SetUpstreamRequested {
        upstream: RemoteBranch,
    },
    // Task 23: the conflict flow. Continue, Skip and Abort act on the
    // operation paused in the snapshot at the moment of the request; if it
    // is not the one paused when they run, they are refused.
    ResolveFileRequested {
        path: RepoPath,
        choice: ResolveChoice,
    },
    ContinueRequested,
    // P2: single-commit cherry-pick and revert of the commit with this id,
    // onto the `HEAD` in the snapshot at the moment of the request.
    CherryPickRequested(Oid),
    RevertRequested(Oid),
    // P7: reset the `HEAD` in the snapshot at the moment of the request to
    // `target` (for example an entry from the reflog), with an undo snapshot.
    ResetRequested {
        target: Oid,
        mode: ResetMode,
    },
    SkipRequested,
    AbortRequested,
    OpCancelRequested(OpId),
    AuthAnswered(OpId, AuthAnswer),
    UndoRequested,
    // Task 15: branches. Names typed by the user are parsed here, in the
    // core; a bad one is refused at once.
    BranchCreateRequested {
        name: String,
        at: BranchStart,
        switch: bool,
    },
    BranchSwitchRequested(BranchName),
    BranchRenameRequested {
        from: BranchName,
        to: String,
    },
    BranchDeleteRequested {
        name: BranchName,
        force: bool,
    },
    // Task 20: stash. Apply, pop and drop name the entry by commit id.
    // P3: tags. A new name and message arrive as typed and are parsed in
    // the core; existing tags are named by their parsed name.
    TagCreateRequested {
        name: String,
        at: BranchStart,
        /// `Some`: an annotated tag with this message.
        message: Option<String>,
    },
    TagDeleteRequested(TagName),
    TagPushRequested {
        remote: String,
        name: TagName,
    },
    RemoteTagDeleteRequested {
        remote: String,
        name: TagName,
    },
    StashSaveRequested {
        message: String,
        include_untracked: bool,
    },
    StashApplyRequested(Oid),
    StashPopRequested(Oid),
    StashDropRequested(Oid),
    RefreshRequested,
    NoticeDismissed,
    /// Task 17: remove `index.lock` that another process left. The UI asks
    /// first; the core still refuses unless the snapshot shows a lock held by
    /// someone else and none of our operations is running.
    RemoveStaleLockRequested,
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
    /// Task 22: the undo points kept from an earlier run, oldest first, after
    /// the shell checked that each one's saved tree is still in the repo. Taken
    /// only while nothing has been recorded in this run.
    UndoStackRestored(Vec<UndoEntry>),
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
    /// Whether the operation rewrites the index (task 15: creating,
    /// renaming and deleting a branch do not).
    pub index_effect: IndexEffect,
    /// How this operation can be reversed, or why it cannot (task 20).
    pub reverse: Reversal,
}

/// Whether an operation changes the index, and so whether undoing it must
/// put the saved index back. An undo that leaves the index alone never
/// clobbers staging done since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexEffect {
    Rewrites,
    Untouched,
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
    /// Task 13: apply the patch to the worktree file `path` (`git apply`,
    /// no `--index`). Refused unless the index is `expect_index` and the
    /// file's bytes hash to `expect_file` (`None`: the file must be absent).
    /// The pre-discard content was saved by the `SnapshotForUndo` before it.
    ApplyPatchToWorktree {
        op: OpId,
        patch: Patch,
        path: RepoPath,
        expect_index: IndexId,
        expect_file: Option<Oid>,
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
    /// Task 17: delete `index.lock` (and only it). Not a write in the sense
    /// of law 12: it exists to lift the lock that blocks writes. The shell
    /// answers with `DiskChanged` so the state is read again.
    RemoveStaleLock,
}

impl Command {
    /// Commands that change `.git` and so must respect the lock (law 12).
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Command::ApplyPatchToIndex { .. }
                | Command::ApplyPatchToWorktree { .. }
                | Command::RunGit { .. }
                | Command::SnapshotForUndo(_)
                | Command::ReplayUndo(_)
        )
    }
}
