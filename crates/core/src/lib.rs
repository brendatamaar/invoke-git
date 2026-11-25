//! Invoke Git's pure core: `step(state, event) -> (state, commands)`.
//!
//! No IO of any kind lives here: no filesystem, network, clock, threads or
//! randomness. The shell runs the [`Command`]s and reports back with
//! [`Event`]s. See `docs/spec/core-design.md`.

pub mod advanced;
pub mod branch;
pub mod clone;
pub mod commit;
pub mod conflict;
pub mod detect;
pub mod event;
pub mod forge;
pub mod ids;
pub mod message;
pub mod notes;
pub mod ops;
pub mod rebase;
pub mod reflog;
pub mod search;
pub mod stash;
pub mod state;
mod step;
pub mod tag;
pub mod undo;
pub mod undo_codec;
pub mod watch;

pub use advanced::{
    AdvancedSnapshot, LFS_SPEC_VERSION, LfsFile, LfsFileState, LfsPointer, LfsRefusal, LfsSnapshot,
    MAX_LFS_POINTER_BYTES, SubmoduleEntry, SubmoduleOp, SubmoduleRefusal, SubmoduleState,
    WorktreeCheckout, WorktreeEntry, WorktreeRefusal, WorktreeSpec, WorktreeTarget,
    classify_submodule_state, format_byte_size, gitattributes_uses_lfs,
    lfs_include_path_is_literal, normalize_worktree_path, parse_lfs_pointer, same_worktree_path,
    validate_worktree_path, worktree_holding_branch,
};
pub use conflict::{
    ConflictContent, ConflictFile, ConflictKind, ConflictRefusal, ContinueGate, FileState,
    Operation, ResolveChoice, Side, StageContent, continue_gate,
};
pub use event::{
    Area, ChangeSet, Command, Event, Expect, HeadTarget, IndexEffect, ReadScope, RefValue,
    UndoPoint,
};
pub use ids::{
    BranchName, IndexId, NameProblem, Oid, OidError, OpId, ReadId, RefName, RepoId, RepoPath,
    TagName,
};
pub use message::{CommitMessage, MessageProblem, SubjectHint, SubjectLevel};
pub use ops::{
    AuthAnswer, AuthKind, AuthPrompt, BranchStart, DiscardRequest, ErrorClass, Failure, FileAction,
    FileRequest, FileVersion, GitArgs, Lease, OpOutcome, Patch, Progress, PullMode, RemoteBranch,
    Selection, StageRequest, WorktreeMode, WriteRequest,
};
pub use rebase::{PlanError, RebaseAction, RebaseOrigin, RebasePause, RebasePlan, RebaseState};
pub use reflog::{ReflogEntry, ReflogTarget, ResetMode};
pub use stash::StashEntry;
pub use state::{
    Action, BranchRefusal, CommitRefusal, Conflicts, DiskState, FileChange, Freshness, Head,
    LockState, Notice, OpPhase, OpQueue, Phase, QueuedOp, Reads, RefsSnapshot, Refusal, RepoState,
    RunningOp, Snapshot, StatusEntry, StatusSnapshot, TagInfo, TagRefusal, Tracking, UndoMoved,
    Upstream,
};
pub use step::{open, step};
pub use undo::{
    DiscardRestore, HeadRestore, Irreversible, MAX_POINTS, RestoreRef, Restored, Reversal,
    Reversible, StashRestore, UndoEntry, UndoPending, UndoReplay, UndoStack,
};
