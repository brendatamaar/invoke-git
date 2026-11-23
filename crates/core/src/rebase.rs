//! Interactive rebase plans and the lifecycle of an app-started rebase.
//! The actual paused operation, including edits and conflicts, is read
//! from git every time and carried by `Phase::Rebasing`.

use std::collections::BTreeSet;

use crate::{BranchName, Head, Oid, OpId, UndoPoint};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseAction {
    Pick,
    Squash,
    Fixup,
    Edit,
    Drop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseControl {
    Continue,
    Skip,
    Abort,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebasePlan {
    upstream: Oid,
    steps: Vec<(Oid, RebaseAction)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    Empty,
    DuplicateCommit,
    MissingSquashTarget,
}

impl RebasePlan {
    /// `upstream` is the parent of the linear range being edited. Every
    /// commit in that range must appear once, including explicit drops.
    pub fn new(upstream: Oid, steps: Vec<(Oid, RebaseAction)>) -> Result<Self, PlanError> {
        if steps.is_empty() {
            return Err(PlanError::Empty);
        }
        let mut commits = BTreeSet::new();
        if steps.iter().any(|(commit, _)| !commits.insert(commit)) {
            return Err(PlanError::DuplicateCommit);
        }
        if matches!(
            steps
                .iter()
                .find(|(_, action)| *action != RebaseAction::Drop),
            Some((_, RebaseAction::Squash | RebaseAction::Fixup))
        ) {
            return Err(PlanError::MissingSquashTarget);
        }
        Ok(Self { upstream, steps })
    }

    pub fn upstream(&self) -> &Oid {
        &self.upstream
    }

    pub fn steps(&self) -> &[(Oid, RebaseAction)] {
        &self.steps
    }
}

/// The branch (or detached HEAD) Git will restore on abort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseOrigin {
    pub branch: Option<BranchName>,
    pub tip: Oid,
}

impl RebaseOrigin {
    pub fn from_head(head: &Head) -> Option<Self> {
        match head {
            Head::Branch { name, oid } => Some(Self {
                branch: Some(name.clone()),
                tip: oid.clone(),
            }),
            Head::Detached(oid) => Some(Self {
                branch: None,
                tip: oid.clone(),
            }),
            Head::Unborn(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RebasePause {
    Conflict,
    Edit,
    /// For example an empty replay, a failed hook or an older apply rebase.
    #[default]
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseUndo {
    pub point: UndoPoint,
    pub tree: Oid,
    pub origin: RebaseOrigin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseFinish {
    Completed,
    Aborted,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseIntent {
    Run,
    AbortAtPause,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseRun {
    Starting(RebaseOrigin),
    Resuming(Box<RebaseUndo>),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RebaseState {
    #[default]
    Idle,
    Editing {
        plan: RebasePlan,
        head: Head,
    },
    Queued(OpId),
    Running {
        run: RebaseRun,
        intent: RebaseIntent,
    },
    Reading {
        saved: Box<RebaseUndo>,
        finish: RebaseFinish,
        intent: RebaseIntent,
    },
    Paused(Box<RebaseUndo>),
    Done,
    Aborted,
}
