//! Reflog types and recovery planning (P7).
//!
//! Pure: no IO. The shell reads `.git/logs/` through `gix` into
//! [`ReflogEntry`] values and executes the [`GitArgs::Reset`] commands planned
//! here through the system `git` CLI.

use crate::event::{Event, HeadTarget, IndexEffect, RefValue, UndoPoint};
use crate::ids::{BranchName, Oid, OpId, RefName};
use crate::ops::{BranchStart, GitArgs};
use crate::state::{BranchRefusal, DiskState, Head, Phase, Refusal};
use crate::undo::Reversal;

/// Which local reflog to inspect (P7): `HEAD` or a local branch.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReflogTarget {
    Head,
    Branch(BranchName),
}

impl ReflogTarget {
    /// The full ref name whose reflog is read (`HEAD` or `refs/heads/<name>`).
    pub fn ref_name(&self) -> RefName {
        match self {
            ReflogTarget::Head => RefName("HEAD".into()),
            ReflogTarget::Branch(name) => name.to_ref(),
        }
    }

    /// Short display label (`HEAD` or `<branch>`).
    pub fn label(&self) -> &str {
        match self {
            ReflogTarget::Head => "HEAD",
            ReflogTarget::Branch(name) => &name.0,
        }
    }

    /// Git reflog selector at 0-based position `index` from newest, e.g.
    /// `HEAD@{0}` or `main@{3}`.
    pub fn selector(&self, index: usize) -> String {
        format!("{}@{{{index}}}", self.label())
    }
}

/// One entry read from a local reflog, newest first (`index == 0` is `@{0}`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReflogEntry {
    /// 0-based position from newest (`0` is `HEAD@{0}` / `<branch>@{0}`).
    pub index: usize,
    /// Selector as git prints it (`HEAD@{0}`, `main@{2}`).
    pub selector: String,
    /// Commit the ref pointed to before this move (`None` when the ref was
    /// created, recorded as an all-zero OID in `.git/logs/`).
    pub old_oid: Option<Oid>,
    /// Commit the ref pointed to after this move.
    pub new_oid: Oid,
    /// Action message recorded by git (e.g. `commit: fix bug`).
    pub message: String,
    /// Committer name recorded in the reflog entry.
    pub actor: String,
    /// Committer email recorded in the reflog entry.
    pub email: String,
    /// Timestamp in Unix seconds.
    pub seconds: i64,
    /// UTC offset in seconds.
    pub offset: i32,
}

impl ReflogEntry {
    /// Event to recover this entry by creating a branch at [`Self::new_oid`].
    pub fn create_branch_event(&self, name: impl Into<String>, switch: bool) -> Event {
        Event::BranchCreateRequested {
            name: name.into(),
            at: BranchStart::Commit(self.new_oid.clone()),
            switch,
        }
    }

    /// Event to recover this entry by resetting `HEAD` to [`Self::new_oid`].
    pub fn reset_event(&self, mode: ResetMode) -> Event {
        Event::ResetRequested {
            target: self.new_oid.clone(),
            mode,
        }
    }

    /// Splits a reflog message on its first `": "` into `(action, detail)` for
    /// display (e.g. `("commit (amend)", "fix typo")`). When there is no `": "`
    /// separator, returns the whole message as the action and `""` as detail.
    pub fn split_message(&self) -> (&str, &str) {
        split_reflog_message(&self.message)
    }
}

/// Splits a reflog message on its first `": "` into `(action, detail)`.
pub fn split_reflog_message(message: &str) -> (&str, &str) {
    match message.split_once(": ") {
        Some((action, detail)) => (action, detail),
        None => (message, ""),
    }
}

/// How a reset moves the current ref and the index (P7).
///
/// Every mode records a reversible undo point (`Reversal::Refs`, law 8) that
/// moves only the targeted ref (law 9) and never rewrites working-tree files:
/// - [`ResetMode::Soft`] moves only the active ref, leaving the index and
///   working tree untouched (`IndexEffect::Untouched`).
/// - [`ResetMode::Mixed`] moves the active ref and resets the index to the
///   target commit's tree (`IndexEffect::Rewrites`), leaving the working tree
///   untouched; undo restores both the ref and the exact pre-reset index tree.
///
/// `--hard` is intentionally not offered here because it rewrites working-tree
/// files, which `Reversal::Refs` does not restore (law 8). Users who want to
/// inspect or work on the target commit's files can create and switch to a
/// recovery branch via [`ReflogEntry::create_branch_event`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResetMode {
    Soft,
    Mixed,
}

/// Checks a reset request against the fresh snapshot `disk` and builds the git
/// invocation and its undo point.
///
/// Enforces:
/// - Phase must be [`Phase::Clean`] (PRD "State machines").
/// - `HEAD` must not be unborn (there must be a commit to record and restore).
/// - `HEAD` must still be on `onto` (law 9: never reset a different branch).
pub fn plan_reset(
    disk: &DiskState,
    op: OpId,
    target: &Oid,
    mode: ResetMode,
    onto: &HeadTarget,
) -> Result<(GitArgs, UndoPoint), Refusal> {
    if disk.phase != Phase::Clean {
        return Err(Refusal::RepoBusy);
    }
    let expected_ref = match &disk.head {
        Head::Branch { name, oid } => RefValue {
            name: name.to_ref(),
            oid: Some(oid.clone()),
        },
        Head::Detached(oid) => RefValue {
            name: RefName("HEAD".into()),
            oid: Some(oid.clone()),
        },
        Head::Unborn(_) => return Err(Refusal::Branch(BranchRefusal::NoCommitsYet)),
    };
    if HeadTarget::of(&disk.head) != *onto {
        return Err(Refusal::BranchChanged);
    }
    let short: String = target.to_string().chars().take(7).collect();
    let label = match mode {
        ResetMode::Soft => format!("Soft reset to {short}"),
        ResetMode::Mixed => format!("Reset to {short}"),
    };
    let index_effect = match mode {
        ResetMode::Soft => IndexEffect::Untouched,
        ResetMode::Mixed => IndexEffect::Rewrites,
    };
    let point = UndoPoint {
        op,
        label,
        head: HeadTarget::of(&disk.head),
        refs: vec![expected_ref.clone()],
        index: disk.index.clone(),
        index_effect,
        reverse: Reversal::Refs,
    };
    let args = GitArgs::Reset {
        target: target.clone(),
        mode,
        expected_ref,
    };
    Ok((args, point))
}
