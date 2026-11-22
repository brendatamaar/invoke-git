//! Invoke Git's pure core: `step(state, event) -> (state, commands)`.
//!
//! No IO of any kind lives here: no filesystem, network, clock, threads or
//! randomness. The shell runs the [`Command`]s and reports back with
//! [`Event`]s. See `docs/spec/core-design.md`.

pub mod branch;
pub mod clone;
pub mod commit;
pub mod conflict;
pub mod detect;
pub mod event;
pub mod ids;
pub mod message;
pub mod ops;
pub mod stash;
pub mod state;
mod step;
pub mod undo;
pub mod watch;

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
};
pub use message::{CommitMessage, MessageProblem, SubjectHint, SubjectLevel};
pub use ops::{
    AuthAnswer, AuthKind, AuthPrompt, BranchStart, DiscardRequest, ErrorClass, Failure, GitArgs,
    Lease, OpOutcome, Patch, Progress, PullMode, RemoteBranch, Selection, StageRequest,
    WriteRequest,
};
pub use stash::StashEntry;
pub use state::{
    Action, BranchRefusal, CommitRefusal, Conflicts, DiskState, FileChange, Freshness, Head,
    LockState, Notice, OpPhase, OpQueue, Phase, QueuedOp, Reads, RefsSnapshot, Refusal, RepoState,
    RunningOp, Snapshot, StatusEntry, StatusSnapshot, Tracking, UndoMoved, Upstream,
};
pub use step::{open, step};
pub use undo::{
    DiscardRestore, HeadRestore, Irreversible, MAX_POINTS, RestoreRef, Reversal, Reversible,
    StashRestore, UndoEntry, UndoPending, UndoReplay, UndoStack,
};
