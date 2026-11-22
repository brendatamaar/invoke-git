//! Conflict resolution (task 23): what is conflicted, what is resolved, and
//! when Continue, Skip and Abort are allowed (`docs/PRD.md` -> "State
//! machines", "Conflict resolution").
//!
//! Everything here is derived from a fresh snapshot (law 11). A file is
//! **Unresolved** while the index still holds stages 1-3 for it, and
//! **Resolved** once stage 0 is back (git remembers those paths in the
//! index's resolve-undo record, which is how the shell finds them).
//!
//! A file edited in the worktree but not staged is still Unresolved: staging
//! is the act of resolving, exactly as in git, and `Continue` would be
//! refused by git itself. The UI offers "Mark resolved"
//! (`ResolveChoice::Manual`), which stages the worktree file as it is.

use crate::ids::RepoPath;
use crate::ops::GitArgs;
use crate::state::{DiskState, Phase};

/// A long operation that can stop on a conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Merge,
    /// The step is part of the identity: a Continue or Skip meant for step 2
    /// must not land on step 3 (a double click must not skip two commits).
    Rebase {
        step: u32,
        total: u32,
    },
    CherryPick,
    Revert,
}

impl Operation {
    /// Skip exists for a rebase only (PRD -> "Repo state").
    pub fn can_skip(self) -> bool {
        matches!(self, Operation::Rebase { .. })
    }
}

impl Phase {
    /// What git is paused on, or `None` when nothing is in progress.
    pub fn operation(&self) -> Option<Operation> {
        match self {
            Phase::Clean => None,
            Phase::Merging { .. } => Some(Operation::Merge),
            Phase::Rebasing { step, total, .. } => Some(Operation::Rebase {
                step: *step,
                total: *total,
            }),
            Phase::CherryPicking { .. } => Some(Operation::CherryPick),
            Phase::Reverting { .. } => Some(Operation::Revert),
        }
    }

    /// The conflict list of a paused operation.
    pub fn conflicts(&self) -> Option<&Conflicts> {
        match self {
            Phase::Clean => None,
            Phase::Merging { conflicts }
            | Phase::CherryPicking { conflicts }
            | Phase::Reverting { conflicts }
            | Phase::Rebasing { conflicts, .. } => Some(conflicts),
        }
    }
}

/// How a path is conflicted: which of the three index stages exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConflictKind {
    /// Stages 1, 2 and 3.
    BothModified,
    /// Stages 2 and 3, no common ancestor.
    BothAdded,
    /// Stages 1 and 3: our side deleted it, theirs changed it.
    DeletedByUs,
    /// Stages 1 and 2: their side deleted it, ours changed it.
    DeletedByThem,
    /// Stage 2 only.
    AddedByUs,
    /// Stage 3 only.
    AddedByThem,
    /// Stage 1 only.
    BothDeleted,
}

impl ConflictKind {
    /// The kind for the stages found in the index, parsed once at the
    /// boundary. No stage at all is not a conflict.
    pub fn from_stages(base: bool, ours: bool, theirs: bool) -> Option<Self> {
        match (base, ours, theirs) {
            (true, true, true) => Some(Self::BothModified),
            (false, true, true) => Some(Self::BothAdded),
            (true, false, true) => Some(Self::DeletedByUs),
            (true, true, false) => Some(Self::DeletedByThem),
            (false, true, false) => Some(Self::AddedByUs),
            (false, false, true) => Some(Self::AddedByThem),
            (true, false, false) => Some(Self::BothDeleted),
            (false, false, false) => None,
        }
    }

    /// Whether stage 2 (ours) exists.
    pub fn has_ours(self) -> bool {
        matches!(
            self,
            Self::BothModified | Self::BothAdded | Self::DeletedByThem | Self::AddedByUs
        )
    }

    /// Whether stage 3 (theirs) exists.
    pub fn has_theirs(self) -> bool {
        matches!(
            self,
            Self::BothModified | Self::BothAdded | Self::DeletedByUs | Self::AddedByThem
        )
    }

    pub fn has_base(self) -> bool {
        matches!(
            self,
            Self::BothModified | Self::DeletedByUs | Self::DeletedByThem | Self::BothDeleted
        )
    }

    pub fn has(self, side: Side) -> bool {
        match side {
            Side::Ours => self.has_ours(),
            Side::Theirs => self.has_theirs(),
        }
    }
}

/// One side of a conflict. `Ours` is index stage 2 (what `HEAD` has), `Theirs`
/// is stage 3. In a rebase `HEAD` is the branch being rebased onto, so
/// "ours" is the upstream and "theirs" is the commit being replayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Ours,
    Theirs,
}

/// Where one conflicted file stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    Unresolved(ConflictKind),
    Resolved,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictFile {
    pub path: RepoPath,
    pub state: FileState,
}

/// The conflicted files of a paused operation, sorted by path, each path once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conflicts {
    files: Vec<ConflictFile>,
}

impl Conflicts {
    /// Builds the list from what the index says: the paths that still have
    /// stages 1-3, and the paths git remembers as resolved. A path that is
    /// both (conflicted again after a resolve) is Unresolved.
    pub fn from_parts(
        unresolved: impl IntoIterator<Item = (RepoPath, ConflictKind)>,
        resolved: impl IntoIterator<Item = RepoPath>,
    ) -> Self {
        let mut files: Vec<ConflictFile> = resolved
            .into_iter()
            .map(|path| ConflictFile {
                path,
                state: FileState::Resolved,
            })
            .chain(unresolved.into_iter().map(|(path, kind)| ConflictFile {
                path,
                state: FileState::Unresolved(kind),
            }))
            .collect();
        // Stable sort: for one path the Unresolved entry comes last, and the
        // dedup below keeps the last of a run.
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut kept: Vec<ConflictFile> = Vec::with_capacity(files.len());
        for file in files {
            match kept.last_mut() {
                Some(last) if last.path == file.path => *last = file,
                _ => kept.push(file),
            }
        }
        Self { files: kept }
    }

    pub fn files(&self) -> &[ConflictFile] {
        &self.files
    }

    pub fn get(&self, path: &RepoPath) -> Option<&ConflictFile> {
        self.files
            .binary_search_by(|file| file.path.cmp(path))
            .ok()
            .map(|index| &self.files[index])
    }

    pub fn unresolved(&self) -> impl Iterator<Item = (&RepoPath, ConflictKind)> {
        self.files.iter().filter_map(|file| match file.state {
            FileState::Unresolved(kind) => Some((&file.path, kind)),
            FileState::Resolved => None,
        })
    }

    pub fn unresolved_count(&self) -> usize {
        self.unresolved().count()
    }

    pub fn resolved_count(&self) -> usize {
        self.files.len() - self.unresolved_count()
    }

    pub fn all_resolved(&self) -> bool {
        self.unresolved_count() == 0
    }
}

/// Whether the operation may continue. The UI enables its Continue button
/// from this; the core re-checks it against the fresh snapshot when the
/// request runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinueGate {
    /// Nothing is paused.
    NotInProgress,
    Blocked {
        unresolved: usize,
    },
    Ready,
}

pub fn continue_gate(phase: &Phase) -> ContinueGate {
    match phase.conflicts() {
        None => ContinueGate::NotInProgress,
        Some(conflicts) => match conflicts.unresolved_count() {
            0 => ContinueGate::Ready,
            unresolved => ContinueGate::Blocked { unresolved },
        },
    }
}

/// How the user resolves one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolveChoice {
    /// Take the `HEAD` side whole (`git checkout --ours`, then stage).
    Ours,
    /// Take the incoming side whole.
    Theirs,
    /// Stage the worktree file as it is: the UI wrote the merged text first.
    /// Also how a file the user deleted by hand is resolved.
    Manual,
}

/// Why a conflict request was turned down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictRefusal {
    /// No merge, rebase, cherry-pick or revert is in progress.
    NotInProgress,
    /// The operation the request was made for is no longer the one paused
    /// (it finished, or a rebase moved to the next step).
    PhaseChanged,
    /// Continue needs every file resolved.
    Unresolved { remaining: usize },
    /// Skip is for a rebase.
    SkipNotAvailable,
    /// The path is not among the conflicted files.
    NotConflicted,
    /// Ours and theirs need the index stages, which a resolved file no
    /// longer has.
    AlreadyResolved,
}

/// Shared check of every conflict request: the operation the user saw must
/// still be the one paused.
pub fn check_operation(disk: &DiskState, expected: Operation) -> Result<(), ConflictRefusal> {
    match disk.phase.operation() {
        None => Err(ConflictRefusal::NotInProgress),
        Some(found) if found != expected => Err(ConflictRefusal::PhaseChanged),
        Some(_) => Ok(()),
    }
}

/// Continue: allowed only when every file is Resolved.
pub fn plan_continue(disk: &DiskState, operation: Operation) -> Result<GitArgs, ConflictRefusal> {
    check_operation(disk, operation)?;
    match continue_gate(&disk.phase) {
        ContinueGate::NotInProgress => Err(ConflictRefusal::NotInProgress),
        ContinueGate::Blocked { unresolved } => Err(ConflictRefusal::Unresolved {
            remaining: unresolved,
        }),
        ContinueGate::Ready => Ok(GitArgs::Continue { operation }),
    }
}

/// Skip the commit being replayed (rebase only).
pub fn plan_skip(disk: &DiskState, operation: Operation) -> Result<GitArgs, ConflictRefusal> {
    check_operation(disk, operation)?;
    if operation.can_skip() {
        Ok(GitArgs::Skip { operation })
    } else {
        Err(ConflictRefusal::SkipNotAvailable)
    }
}

/// Abort: allowed whenever something is paused, whatever the files say.
pub fn plan_abort(disk: &DiskState, operation: Operation) -> Result<GitArgs, ConflictRefusal> {
    check_operation(disk, operation)?;
    Ok(GitArgs::Abort { operation })
}

/// Resolving one file, against the snapshot the write runs on. A side that
/// does not exist (the other side deleted the file) is taken by removing the
/// file.
pub fn plan_resolve(
    disk: &DiskState,
    path: &RepoPath,
    choice: ResolveChoice,
) -> Result<GitArgs, ConflictRefusal> {
    let conflicts = disk
        .phase
        .conflicts()
        .ok_or(ConflictRefusal::NotInProgress)?;
    let file = conflicts.get(path).ok_or(ConflictRefusal::NotConflicted)?;
    let take = |side: Side| match file.state {
        FileState::Resolved => Err(ConflictRefusal::AlreadyResolved),
        FileState::Unresolved(kind) if kind.has(side) => Ok(GitArgs::ResolveTakeSide {
            path: path.clone(),
            side,
        }),
        FileState::Unresolved(_) => Ok(GitArgs::ResolveRemove { path: path.clone() }),
    };
    match choice {
        ResolveChoice::Ours => take(Side::Ours),
        ResolveChoice::Theirs => take(Side::Theirs),
        ResolveChoice::Manual => Ok(GitArgs::ResolveStage { path: path.clone() }),
    }
}

/// What the UI shows for one side of a conflicted file. The shell reads it
/// from the index stage's blob (`docs/design/screens/Conflict.dc.html`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageContent {
    /// The stage does not exist (the file was added or deleted on a side).
    Absent,
    /// Valid UTF-8 without NUL bytes, kept byte for byte (CRLF stays CRLF).
    Text(String),
    /// Anything else: shown as "binary file", never merged line by line.
    Binary { size: u64 },
}

/// The three versions of a conflicted file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictContent {
    pub path: RepoPath,
    pub base: StageContent,
    pub ours: StageContent,
    pub theirs: StageContent,
}

impl ConflictContent {
    /// Binary on any side: the UI offers only "use ours" and "use theirs".
    pub fn is_binary(&self) -> bool {
        [&self.base, &self.ours, &self.theirs]
            .into_iter()
            .any(|side| matches!(side, StageContent::Binary { .. }))
    }
}
