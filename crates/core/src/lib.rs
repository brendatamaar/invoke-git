//! Invoke Git's pure core: `step(state, event) -> (state, commands)`.
//!
//! No IO of any kind lives here: no filesystem, network, clock, threads or
//! randomness. The shell runs the [`Command`]s and reports back with
//! [`Event`]s. See `docs/spec/core-design.md`.

pub mod branch;
pub mod clone;
pub mod detect;
pub mod event;
pub mod ids;
pub mod ops;
pub mod state;
mod step;
pub mod undo;
pub mod watch;

pub use event::{
    Area, ChangeSet, Command, Event, Expect, HeadTarget, IndexEffect, ReadScope, RefValue,
    UndoPoint,
};
pub use ids::{
    BranchName, IndexId, NameProblem, Oid, OidError, OpId, ReadId, RefName, RepoId, RepoPath,
};
pub use ops::{
    AuthAnswer, AuthKind, AuthPrompt, BranchStart, ErrorClass, Failure, GitArgs, OpOutcome, Patch,
    Progress, Selection, StageRequest, WriteRequest,
};
pub use state::{
    Action, BranchRefusal, Conflicts, DiskState, FileChange, Freshness, Head, LockState, Notice,
    OpPhase, OpQueue, Phase, QueuedOp, Reads, RefsSnapshot, Refusal, RepoState, RunningOp,
    Snapshot, StatusEntry, StatusSnapshot, Tracking, UndoMoved, Upstream,
};
pub use step::{open, step};
pub use undo::{
    HeadRestore, Irreversible, MAX_POINTS, RestoreRef, Reversible, UndoEntry, UndoPending,
    UndoReplay, UndoStack,
};
