//! Branch operations (task 15): checking a request against the fresh
//! snapshot, and building the git run and the undo point for it.
//!
//! Everything the user can get wrong is refused here with a stated reason,
//! so the shell only ever runs a request that was valid on the disk it
//! read. What cannot be known from the snapshot (whether a switch would
//! overwrite local changes, whether a branch is fully merged) is left to
//! git, which refuses before changing anything; the shell classifies that.

use crate::event::{HeadTarget, IndexEffect, RefValue, UndoPoint};
use crate::ids::{BranchName, Oid, OpId, RefName};
use crate::ops::{BranchStart, GitArgs, WriteRequest};
use crate::state::{Action, BranchRefusal, DiskState, Head, Phase, Refusal};
use crate::undo::Reversal;

/// The request's name for notices; `None` when it is not about branches.
pub fn action_of(request: &WriteRequest) -> Option<Action> {
    match request {
        WriteRequest::CreateBranch { .. } => Some(Action::CreateBranch),
        WriteRequest::SwitchBranch { .. } => Some(Action::SwitchBranch),
        WriteRequest::RenameBranch { .. } => Some(Action::RenameBranch),
        WriteRequest::DeleteBranch { .. } => Some(Action::DeleteBranch),
        WriteRequest::Commit { .. }
        | WriteRequest::Undo
        | WriteRequest::Stage(_)
        | WriteRequest::Unstage(_)
        | WriteRequest::Discard(_)
        | WriteRequest::StashSave { .. }
        | WriteRequest::StashApply { .. }
        | WriteRequest::StashPop { .. }
        | WriteRequest::StashDrop { .. }
        // P3: planned by `tag::plan`.
        | WriteRequest::CreateTag { .. }
        | WriteRequest::DeleteTag { .. }
        | WriteRequest::PushTag { .. }
        | WriteRequest::DeleteRemoteTag { .. }
        | WriteRequest::Fetch { .. }
        | WriteRequest::Pull { .. }
        | WriteRequest::Push { .. }
        | WriteRequest::SetUpstream { .. }
        // Task 23.
        | WriteRequest::ResolveFile { .. }
        | WriteRequest::Continue { .. }
        | WriteRequest::Skip { .. }
        | WriteRequest::Abort { .. }
        // P2.
        | WriteRequest::CherryPick { .. }
        | WriteRequest::Revert { .. } => None,
    }
}

/// Checks a branch request against `disk` and builds the run and its undo
/// point. `None` when the request is not about branches.
pub fn plan(
    request: &WriteRequest,
    disk: &DiskState,
    op: OpId,
) -> Option<Result<(GitArgs, UndoPoint), Refusal>> {
    action_of(request)?;
    Some(check(request, disk, op))
}

fn refuse<T>(why: BranchRefusal) -> Result<T, Refusal> {
    Err(Refusal::Branch(why))
}

fn check(
    request: &WriteRequest,
    disk: &DiskState,
    op: OpId,
) -> Result<(GitArgs, UndoPoint), Refusal> {
    // PRD "State machines": during a merge, rebase, cherry-pick or revert
    // only resolve, continue, skip and abort are allowed.
    if disk.phase != Phase::Clean {
        return Err(Refusal::RepoBusy);
    }
    let Some(head_oid) = head_oid(&disk.head) else {
        return refuse(BranchRefusal::NoCommitsYet);
    };
    let tip_of = |name: &BranchName| disk.refs.refs.get(&name.to_ref());
    let is_current = |name: &BranchName| matches!(&disk.head, Head::Branch { name: current, .. } if current == name);
    let point = |label: String, refs: Vec<RefValue>, index_effect: IndexEffect| UndoPoint {
        op,
        label,
        head: HeadTarget::of(&disk.head),
        refs,
        index: disk.index.clone(),
        index_effect,
        reverse: Reversal::Refs,
    };
    let at = |name: &BranchName, oid: Option<&Oid>| RefValue {
        name: name.to_ref(),
        oid: oid.cloned(),
    };
    match request {
        WriteRequest::CreateBranch {
            name,
            at: start,
            switch,
        } => {
            if tip_of(name).is_some() {
                return refuse(BranchRefusal::AlreadyExists(name.clone()));
            }
            if let Some(other) = conflicting(disk, name) {
                return refuse(BranchRefusal::NameConflict(other));
            }
            let start = match start {
                BranchStart::Head => head_oid.clone(),
                BranchStart::Commit(oid) => oid.clone(),
            };
            let mut refs = Vec::new();
            if *switch {
                refs.push(head_ref(&disk.head));
            }
            refs.push(at(name, None));
            // Starting where `HEAD` is leaves every file as it is.
            let effect = if *switch && start != *head_oid {
                IndexEffect::Rewrites
            } else {
                IndexEffect::Untouched
            };
            let label = if *switch {
                format!("Create and switch to '{}'", name.0)
            } else {
                format!("Create branch '{}'", name.0)
            };
            Ok((
                GitArgs::CreateBranch {
                    name: name.clone(),
                    at: start,
                    switch: *switch,
                },
                point(label, refs, effect),
            ))
        }
        WriteRequest::SwitchBranch { name } => {
            let Some(tip) = tip_of(name) else {
                return refuse(BranchRefusal::NoSuchBranch(name.clone()));
            };
            if is_current(name) {
                return refuse(BranchRefusal::IsCurrent(name.clone()));
            }
            let refs = vec![head_ref(&disk.head), at(name, Some(tip))];
            let effect = if tip == head_oid {
                IndexEffect::Untouched
            } else {
                IndexEffect::Rewrites
            };
            Ok((
                GitArgs::SwitchBranch {
                    name: name.clone(),
                    tip: tip.clone(),
                },
                point(format!("Switch to '{}'", name.0), refs, effect),
            ))
        }
        WriteRequest::RenameBranch { from, to } => {
            let Some(tip) = tip_of(from) else {
                return refuse(BranchRefusal::NoSuchBranch(from.clone()));
            };
            if from == to {
                return refuse(BranchRefusal::SameName);
            }
            if tip_of(to).is_some() {
                return refuse(BranchRefusal::AlreadyExists(to.clone()));
            }
            if let Some(other) = conflicting(disk, to) {
                return refuse(BranchRefusal::NameConflict(other));
            }
            let refs = vec![at(from, Some(tip)), at(to, None)];
            Ok((
                GitArgs::RenameBranch {
                    from: from.clone(),
                    tip: tip.clone(),
                    to: to.clone(),
                },
                point(
                    format!("Rename '{}' to '{}'", from.0, to.0),
                    refs,
                    IndexEffect::Untouched,
                ),
            ))
        }
        WriteRequest::DeleteBranch { name, force } => {
            let Some(tip) = tip_of(name) else {
                return refuse(BranchRefusal::NoSuchBranch(name.clone()));
            };
            if is_current(name) {
                return refuse(BranchRefusal::IsCurrent(name.clone()));
            }
            Ok((
                GitArgs::DeleteBranch {
                    name: name.clone(),
                    tip: tip.clone(),
                    force: *force,
                },
                point(
                    format!("Delete branch '{}'", name.0),
                    vec![at(name, Some(tip))],
                    IndexEffect::Untouched,
                ),
            ))
        }
        WriteRequest::Commit { .. }
        | WriteRequest::Undo
        | WriteRequest::Stage(_)
        | WriteRequest::Unstage(_)
        | WriteRequest::Discard(_)
        | WriteRequest::StashSave { .. }
        | WriteRequest::StashApply { .. }
        | WriteRequest::StashPop { .. }
        | WriteRequest::StashDrop { .. }
        // P3: planned by `tag::plan`.
        | WriteRequest::CreateTag { .. }
        | WriteRequest::DeleteTag { .. }
        | WriteRequest::PushTag { .. }
        | WriteRequest::DeleteRemoteTag { .. }
        | WriteRequest::Fetch { .. }
        | WriteRequest::Pull { .. }
        | WriteRequest::Push { .. }
        | WriteRequest::SetUpstream { .. }
        // Task 23.
        | WriteRequest::ResolveFile { .. }
        | WriteRequest::Continue { .. }
        | WriteRequest::Skip { .. }
        | WriteRequest::Abort { .. }
        // P2.
        | WriteRequest::CherryPick { .. }
        | WriteRequest::Revert { .. } => Err(Refusal::NotYetSupported { task: 15 }),
    }
}

fn head_oid(head: &Head) -> Option<&Oid> {
    match head {
        Head::Branch { oid, .. } | Head::Detached(oid) => Some(oid),
        Head::Unborn(_) => None,
    }
}

/// The ref a branch operation may move for the current head: the branch, or
/// `HEAD` itself when detached.
fn head_ref(head: &Head) -> RefValue {
    match head {
        Head::Branch { name, oid } => RefValue {
            name: name.to_ref(),
            oid: Some(oid.clone()),
        },
        Head::Detached(oid) => RefValue {
            name: RefName("HEAD".into()),
            oid: Some(oid.clone()),
        },
        Head::Unborn(name) => RefValue {
            name: name.to_ref(),
            oid: None,
        },
    }
}

/// A local branch that cannot live beside `name`: one that is a folder of
/// it, or that it is a folder of (`a` and `a/b`).
fn conflicting(disk: &DiskState, name: &BranchName) -> Option<BranchName> {
    let folder_of = |outer: &str, inner: &str| {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    disk.refs
        .refs
        .keys()
        .filter_map(|full| full.0.strip_prefix("refs/heads/"))
        .find(|existing| folder_of(existing, &name.0) || folder_of(&name.0, existing))
        .map(|existing| BranchName(existing.to_owned()))
}
