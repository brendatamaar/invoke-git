//! Repo state. Everything under [`DiskState`] is derived from `.git` on disk
//! and only ever replaced wholesale by a fresh read (law 11).

use std::collections::{BTreeMap, VecDeque};

use crate::ids::{BranchName, IndexId, Oid, OpId, ReadId, RefName, RepoId, RepoPath};
use crate::ops::{AuthPrompt, Failure, OpOutcome, Progress, WriteRequest};
use crate::undo::{Irreversible, UndoStack};

/// The state of one open repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoState {
    pub id: RepoId,
    /// The last snapshot read from disk, and whether it is still current.
    pub snapshot: Snapshot,
    pub reads: Reads,
    pub ops: OpQueue,
    pub undo: UndoStack,
    /// The newest thing the user should be told; replaced by the next one.
    pub notice: Option<Notice>,
}

/// What is known about the disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Snapshot {
    /// Nothing read yet (the repo was just opened).
    NotRead,
    Read {
        disk: DiskState,
        read: ReadId,
    },
}

impl Snapshot {
    pub fn disk(&self) -> Option<&DiskState> {
        match self {
            Snapshot::NotRead => None,
            Snapshot::Read { disk, .. } => Some(disk),
        }
    }
}

/// Read bookkeeping: ids are handed out in request order, and the snapshot
/// is current only when the newest requested read has landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reads {
    pub next: ReadId,
    pub freshness: Freshness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Freshness {
    /// The snapshot reflects the newest read requested.
    Fresh,
    /// A newer read was requested and has not landed yet.
    Pending { latest: ReadId },
    /// The newest read failed; the snapshot may be out of date.
    Failed { read: ReadId },
}

/// Everything read from `.git` and the worktree in one go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskState {
    pub head: Head,
    pub phase: Phase,
    pub index: IndexId,
    pub status: StatusSnapshot,
    pub refs: RefsSnapshot,
    pub lock: LockState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    /// `HEAD` holds `ref: refs/heads/<name>` and the branch exists.
    Branch { name: BranchName, oid: Oid },
    /// `HEAD` holds a bare object id.
    Detached(Oid),
    /// `HEAD` names a branch that has no commits yet.
    Unborn(BranchName),
}

/// What git itself is in the middle of (first matching rule wins, see
/// `docs/spec/core-design.md` → "Repo state detection").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Clean,
    Merging {
        conflicts: Conflicts,
    },
    Rebasing {
        step: u32,
        total: u32,
        conflicts: Conflicts,
    },
    CherryPicking {
        conflicts: Conflicts,
    },
    Reverting {
        conflicts: Conflicts,
    },
}

/// Files with index stages 1–3.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conflicts {
    pub paths: Vec<RepoPath>,
}

/// Index and worktree changes. Task 11 extends this into the status tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub entries: Vec<StatusEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: RepoPath,
    /// Index vs HEAD.
    pub staged: Option<FileChange>,
    /// Worktree vs index.
    pub unstaged: Option<FileChange>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
    Renamed { from: RepoPath },
    TypeChanged,
    Untracked,
}

/// Every ref and the object it points at. Task 15 adds upstreams and
/// ahead/behind counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefsSnapshot {
    pub refs: BTreeMap<RefName, Oid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    Free,
    /// `index.lock` exists and one of our own git processes created it.
    HeldByUs,
    /// `index.lock` exists and someone else holds it: all writes wait.
    HeldByOther,
}

/// One write at a time per repo; the rest wait in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpQueue {
    pub next: OpId,
    pub running: Option<RunningOp>,
    pub queued: VecDeque<QueuedOp>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedOp {
    pub id: OpId,
    pub request: WriteRequest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningOp {
    pub id: OpId,
    pub request: WriteRequest,
    pub phase: OpPhase,
}

/// The lifecycle of a running operation (PRD → "State machines"). Finished
/// operations leave the queue; their outcome becomes a [`Notice`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpPhase {
    Running {
        progress: Option<Progress>,
    },
    /// Waiting for the user to answer a credential prompt.
    AwaitingAuth(AuthPrompt),
    /// Cancel was sent; waiting for the shell to report the end.
    Cancelling,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Refused {
        action: Action,
        reason: Refusal,
    },
    OpEnded {
        op: OpId,
        outcome: OpOutcome,
    },
    ReadFailed {
        read: ReadId,
        failure: Failure,
    },
    /// A push the remote turned down (non-fast-forward, or a lease that no
    /// longer held). An expected outcome, not an error: the UI offers
    /// `FetchRequested` and then `PullRequested` with `PullMode::Rebase`.
    PushRejected {
        op: OpId,
        failure: Failure,
    },
}

/// A user request, named for notices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Stage,
    Unstage,
    Commit,
    Push,
    Undo,
    // Task 17.
    Fetch,
    Pull,
    SetUpstream,
}

/// Why a request was turned down. Every refusal states one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The skeleton does not handle this yet; the named backlog task adds it.
    NotYetSupported {
        task: u8,
    },
    NothingToUndo,
    /// The newest operation cannot be undone (for example a push).
    NotUndoable(Irreversible),
    /// Something else moved what the operation left; undo never overwrites
    /// someone else's change.
    Changed(UndoMoved),
    /// A merge, rebase, cherry-pick or revert is in progress.
    RepoBusy,
    // Task 17.
    /// Nothing has been read from disk yet, so there is nothing to act on.
    NotRead,
    /// The request needs a checked-out branch; `HEAD` is detached or unborn.
    NoBranch,
    /// A force push needs the remote value the user reviewed; there is no
    /// remote-tracking ref for the target, so it is refused (law 10).
    NoRemoteValue(RefName),
    /// The branch checked out is not the one the request was made for.
    BranchChanged,
}

/// What moved since the operation, as found when an undo was planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoMoved {
    /// `HEAD` now points somewhere else (another branch, or detached).
    Head,
    Ref(RefName),
    Index,
}

impl RepoState {
    /// Freshly opened: nothing read yet, no operations.
    pub fn new(id: RepoId) -> Self {
        Self {
            id,
            snapshot: Snapshot::NotRead,
            reads: Reads {
                next: ReadId(0),
                freshness: Freshness::Fresh,
            },
            ops: OpQueue {
                next: OpId(0),
                running: None,
                queued: VecDeque::new(),
            },
            undo: UndoStack::default(),
            notice: None,
        }
    }

    /// The fresh snapshot, if the newest requested read has landed.
    pub fn fresh_disk(&self) -> Option<&DiskState> {
        match self.reads.freshness {
            Freshness::Fresh => self.snapshot.disk(),
            Freshness::Pending { .. } | Freshness::Failed { .. } => None,
        }
    }
}
