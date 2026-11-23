//! Tag operations (P3): checking a request against the fresh snapshot, and
//! building the git run and the undo point for it.
//!
//! Creating and deleting a local tag move one ref under `refs/tags/` and
//! never touch the index or the worktree, so both are undoable by restoring
//! that ref (law 8). Pushing a tag and deleting one on a remote change
//! someone else's repository: they are irreversible (law 9) and, like a
//! push, are left to the caller to record with an irreversible marker.

use crate::event::{HeadTarget, IndexEffect, RefValue, UndoPoint};
use crate::ids::{Oid, OpId, TagName};
use crate::ops::{BranchStart, GitArgs, WriteRequest};
use crate::state::{Action, DiskState, Head, Phase, Refusal, TagRefusal};
use crate::undo::Reversal;

/// The request's name for notices; `None` when it is not about tags.
pub fn action_of(request: &WriteRequest) -> Option<Action> {
    match request {
        WriteRequest::CreateTag { .. } => Some(Action::CreateTag),
        WriteRequest::DeleteTag { .. } => Some(Action::DeleteTag),
        WriteRequest::PushTag { .. } => Some(Action::PushTag),
        WriteRequest::DeleteRemoteTag { .. } => Some(Action::DeleteRemoteTag),
        _ => None,
    }
}

/// Whether the request talks to a remote (and so is irreversible).
pub fn is_remote(request: &WriteRequest) -> bool {
    matches!(
        request,
        WriteRequest::PushTag { .. } | WriteRequest::DeleteRemoteTag { .. }
    )
}

/// What a checked tag request becomes: the run, and the label an undo point
/// (or its irreversible marker) carries. `undo_refs` is empty for remote
/// requests.
pub struct Planned {
    pub args: GitArgs,
    pub label: String,
    pub undo_refs: Vec<RefValue>,
    pub head: HeadTarget,
    pub index: crate::ids::IndexId,
}

impl Planned {
    /// The undo point of a local tag request.
    pub fn point(&self, op: OpId) -> UndoPoint {
        UndoPoint {
            op,
            label: self.label.clone(),
            head: self.head.clone(),
            refs: self.undo_refs.clone(),
            index: self.index.clone(),
            index_effect: IndexEffect::Untouched,
            reverse: Reversal::Refs,
        }
    }
}

fn refuse<T>(why: TagRefusal) -> Result<T, Refusal> {
    Err(Refusal::Tag(why))
}

/// Checks a tag request against `disk`. `None` when it is not about tags.
pub fn plan(request: &WriteRequest, disk: &DiskState) -> Option<Result<Planned, Refusal>> {
    action_of(request)?;
    Some(check(request, disk))
}

fn check(request: &WriteRequest, disk: &DiskState) -> Result<Planned, Refusal> {
    // A paused merge, rebase, cherry-pick or revert allows only resolve,
    // continue, skip and abort (PRD "State machines").
    if disk.phase != Phase::Clean {
        return Err(Refusal::RepoBusy);
    }
    let tip_of = |name: &TagName| disk.refs.refs.get(&name.to_ref());
    let planned = |args, label, undo_refs| Planned {
        args,
        label,
        undo_refs,
        head: HeadTarget::of(&disk.head),
        index: disk.index.clone(),
    };
    let at = |name: &TagName, oid: Option<&Oid>| RefValue {
        name: name.to_ref(),
        oid: oid.cloned(),
    };
    match request {
        WriteRequest::CreateTag {
            name,
            at: start,
            message,
        } => {
            let start = match start {
                BranchStart::Head => match &disk.head {
                    Head::Branch { oid, .. } | Head::Detached(oid) => oid.clone(),
                    Head::Unborn(_) => return refuse(TagRefusal::NoCommitsYet),
                },
                BranchStart::Commit(oid) => oid.clone(),
            };
            if tip_of(name).is_some() {
                return refuse(TagRefusal::AlreadyExists(name.clone()));
            }
            if let Some(other) = conflicting(disk, name) {
                return refuse(TagRefusal::NameConflict(other));
            }
            let kind = if message.is_some() { "annotated tag" } else { "tag" };
            Ok(planned(
                GitArgs::CreateTag {
                    name: name.clone(),
                    at: start,
                    message: message.as_ref().map(|m| m.as_str().to_owned()),
                },
                format!("Create {kind} '{}'", name.0),
                vec![at(name, None)],
            ))
        }
        WriteRequest::DeleteTag { name } => {
            let Some(tip) = tip_of(name) else {
                return refuse(TagRefusal::NoSuchTag(name.clone()));
            };
            Ok(planned(
                GitArgs::DeleteTag {
                    name: name.clone(),
                    tip: tip.clone(),
                },
                format!("Delete tag '{}'", name.0),
                vec![at(name, Some(tip))],
            ))
        }
        WriteRequest::PushTag { remote, name } => {
            if remote.is_empty() {
                return refuse(TagRefusal::NoRemote);
            }
            let Some(tip) = tip_of(name) else {
                return refuse(TagRefusal::NoSuchTag(name.clone()));
            };
            Ok(planned(
                GitArgs::PushTag {
                    remote: remote.clone(),
                    name: name.clone(),
                    tip: tip.clone(),
                },
                format!("Push tag '{}' to {remote}", name.0),
                Vec::new(),
            ))
        }
        WriteRequest::DeleteRemoteTag { remote, name } => {
            if remote.is_empty() {
                return refuse(TagRefusal::NoRemote);
            }
            Ok(planned(
                GitArgs::DeleteRemoteTag {
                    remote: remote.clone(),
                    name: name.clone(),
                },
                format!("Delete tag '{}' on {remote}", name.0),
                Vec::new(),
            ))
        }
        _ => Err(Refusal::NotYetSupported { task: 0 }),
    }
}

/// A local tag that cannot live beside `name`: one that is a folder of it,
/// or that it is a folder of (`a` and `a/b`).
fn conflicting(disk: &DiskState, name: &TagName) -> Option<TagName> {
    let folder_of = |outer: &str, inner: &str| {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    disk.refs
        .refs
        .keys()
        .filter_map(|full| full.0.strip_prefix("refs/tags/"))
        .find(|existing| folder_of(existing, &name.0) || folder_of(&name.0, existing))
        .map(|existing| TagName(existing.to_owned()))
}
