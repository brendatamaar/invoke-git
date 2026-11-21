//! Invoke Git's pure core: `step(state, event) -> (state, commands)`.
//!
//! No IO of any kind lives here: no filesystem, network, clock, threads or
//! randomness. The shell runs the [`Command`]s and reports back with
//! [`Event`]s. See `docs/spec/core-design.md`.

pub mod clone;
pub mod detect;
pub mod event;
pub mod ids;
pub mod ops;
pub mod stash;
pub mod state;
mod step;
pub mod undo;
pub mod watch;

pub use event::{
    Area, ChangeSet, Command, Event, Expect, HeadTarget, ReadScope, RefValue, UndoPoint,
};
pub use ids::{BranchName, IndexId, Oid, OidError, OpId, ReadId, RefName, RepoId, RepoPath};
pub use ops::{
    AuthAnswer, AuthKind, AuthPrompt, ErrorClass, Failure, GitArgs, OpOutcome, Patch, Progress,
    Selection, WriteRequest,
};
pub use stash::StashEntry;
pub use state::{
    Action, Conflicts, DiskState, FileChange, Freshness, Head, LockState, Notice, OpPhase, OpQueue,
    Phase, QueuedOp, Reads, RefsSnapshot, Refusal, RepoState, RunningOp, Snapshot, StatusEntry,
    StatusSnapshot, UndoMoved,
};
pub use step::{open, step};
pub use undo::{
    Irreversible, MAX_POINTS, RestoreRef, Reversal, Reversible, StashRestore, UndoEntry,
    UndoPending, UndoReplay, UndoStack,
};
