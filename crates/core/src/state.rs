//! Repo state. Everything under [`DiskState`] is derived from `.git` on disk
//! and only ever replaced wholesale by a fresh read (law 11).

use std::collections::{BTreeMap, VecDeque};

use crate::ids::{BranchName, IndexId, NameProblem, Oid, OpId, ReadId, RefName, RepoId, RepoPath};
use crate::ops::{AuthPrompt, Failure, OpOutcome, Progress, WriteRequest};
use crate::stash::StashEntry;
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
    /// The stash list, newest first (`stash@{0}` is first). Task 20.
    pub stashes: Vec<StashEntry>,
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

/// Index and worktree changes, one entry per path.
///
/// `entries` is sorted by path (byte order), so the files under one
/// directory are always a contiguous run and a tree view can be built in a
/// single pass. Build one with [`StatusSnapshot::from_entries`]; the shell
/// reads it fresh with every `StateRead` (law 11).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub entries: Vec<StatusEntry>,
}

impl StatusSnapshot {
    /// Sorts by path and merges entries of one path (staged and unstaged
    /// halves arrive separately from git). Entries without any change are
    /// dropped.
    pub fn from_entries(entries: impl IntoIterator<Item = StatusEntry>) -> Self {
        let mut merged: BTreeMap<RepoPath, (Option<FileChange>, Option<FileChange>)> =
            BTreeMap::new();
        for entry in entries {
            let slot = merged.entry(entry.path).or_default();
            slot.0 = slot.0.take().or(entry.staged);
            slot.1 = slot.1.take().or(entry.unstaged);
        }
        Self {
            entries: merged
                .into_iter()
                .filter(|(_, (staged, unstaged))| staged.is_some() || unstaged.is_some())
                .map(|(path, (staged, unstaged))| StatusEntry {
                    path,
                    staged,
                    unstaged,
                })
                .collect(),
        }
    }

    /// Paths with a change in the index, in tree order.
    pub fn staged(&self) -> impl Iterator<Item = (&RepoPath, &FileChange)> {
        self.entries
            .iter()
            .filter_map(|e| e.staged.as_ref().map(|c| (&e.path, c)))
    }

    /// Paths with a change in the worktree (untracked files included), in
    /// tree order.
    pub fn unstaged(&self) -> impl Iterator<Item = (&RepoPath, &FileChange)> {
        self.entries
            .iter()
            .filter_map(|e| e.unstaged.as_ref().map(|c| (&e.path, c)))
    }
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

/// Every ref and the object it points at, and each local branch's upstream
/// with how far apart they are.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RefsSnapshot {
    pub refs: BTreeMap<RefName, Oid>,
    /// Only branches that have an upstream configured (task 15).
    pub upstreams: BTreeMap<BranchName, Upstream>,
}

/// The branch a local branch tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upstream {
    /// For example `refs/remotes/origin/main`.
    pub name: RefName,
    pub tracking: Tracking,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracking {
    /// The upstream is configured but its ref does not exist (deleted on
    /// the remote and pruned, or never fetched).
    Gone,
    /// Commits only on the branch, and only on the upstream.
    Apart { ahead: u32, behind: u32 },
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
    // Task 15.
    CreateBranch,
    SwitchBranch,
    RenameBranch,
    DeleteBranch,
    // Task 20.
    StashSave,
    StashApply,
    StashPop,
    StashDrop,
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
    /// Task 11: the diff the patch was built from is older than the index
    /// on disk. Refresh the diff and select again.
    StaleDiff,
    /// Task 11: the selection produced no change to apply.
    NothingSelected,
    /// The newest operation cannot be undone (for example a push).
    NotUndoable(Irreversible),
    /// Something else moved what the operation left; undo never overwrites
    /// someone else's change.
    Changed(UndoMoved),
    /// A merge, rebase, cherry-pick or revert is in progress.
    RepoBusy,
    /// Task 20: the stash is no longer in the stash list (dropped, popped or
    /// saved over by someone else). Stashes are addressed by commit id, so
    /// a request never lands on a different entry that took its place.
    StashGone(Oid),
    /// A branch request that cannot run on the snapshot it was checked
    /// against (task 15).
    Branch(BranchRefusal),
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

/// Why a branch request was turned down (task 15).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchRefusal {
    InvalidName(NameProblem),
    /// `HEAD` names a branch with no commits: nothing to start from, switch
    /// to or rename.
    NoCommitsYet,
    AlreadyExists(BranchName),
    /// A branch whose name is a folder of the new one or the other way
    /// round (`a` and `a/b` cannot both exist).
    NameConflict(BranchName),
    NoSuchBranch(BranchName),
    /// Switching to, or deleting, the branch that is checked out.
    IsCurrent(BranchName),
    /// Renaming a branch to its own name.
    SameName,
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
