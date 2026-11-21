//! The undo stack (law 8, law 9): what each write recorded before it ran,
//! what it left behind, and the plan for putting the old values back.
//!
//! Everything here is pure. The shell takes the snapshots and replays the
//! plans (`docs/spec/core-design.md` -> "Undo design").

use std::collections::VecDeque;

use crate::event::{Expect, HeadTarget, RefValue, UndoPoint};
use crate::ids::{IndexId, Oid, OpId, RefName};
use crate::state::{DiskState, Head, Phase, Refusal, UndoMoved};

/// Undo points kept per repo; the oldest is dropped (and pruned on disk)
/// when a new one arrives.
pub const MAX_POINTS: usize = 50;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UndoStack {
    /// Oldest first; the last entry is what an undo would reverse.
    pub entries: VecDeque<UndoEntry>,
    pub pending: UndoPending,
}

/// A finished operation that can (or, for a push, cannot) be undone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoEntry {
    /// What was true before the operation. `point.op` names the saved tree
    /// under `refs/invoke-git/undo/<op>`.
    pub point: UndoPoint,
    /// The saved index tree.
    pub tree: Oid,
    /// The same refs as `point.refs`, as the operation left them. If disk
    /// differs at undo time, someone else moved them and undo refuses.
    pub after_refs: Vec<RefValue>,
    /// The index as the operation left it.
    pub after_index: IndexId,
    pub reversible: Reversible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reversible {
    Yes,
    No(Irreversible),
}

/// Why an operation cannot be undone. Such an entry still sits on the
/// stack, so an undo explains itself instead of silently reversing the
/// operation before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Irreversible {
    /// It changed the remote.
    Push,
}

/// Where the point of the operation in flight is in its life.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UndoPending {
    #[default]
    Idle,
    /// `SnapshotForUndo` was sent; the saved tree is not known yet.
    Recording { point: UndoPoint },
    /// The tree is saved and the operation is running.
    Taken { point: UndoPoint, tree: Oid },
    /// The operation succeeded; the point waits for a fresh read to learn
    /// what the operation left behind.
    Settling { point: UndoPoint, tree: Oid },
    /// An undo is running with this entry taken off the stack.
    Replaying { entry: UndoEntry },
    /// An undo succeeded; the next fresh read re-bases the new top entry's
    /// index id (the index was rebuilt from a tree, so its checksum differs
    /// from what the earlier operation left).
    Rewinding,
}

/// Ref and index restore, with everything the shell must check first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoReplay {
    pub op: OpId,
    pub refs: Vec<RestoreRef>,
    /// Saved index tree to read back.
    pub tree: Oid,
    /// Head and index as read just now; the shell refuses if they moved.
    pub expect: Expect,
    // Task 13 adds the discarded blobs to write back to their paths here.
}

/// One ref to restore, atomically: only if it still holds `expect`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoreRef {
    pub name: RefName,
    /// What the ref must hold now (`None`: must not exist).
    pub expect: Option<Oid>,
    /// What to put back (`None`: delete the ref).
    pub restore: Option<Oid>,
}

impl UndoStack {
    /// Records an operation that cannot be undone (a push, task 17) so an
    /// undo stops at it with a reason. Returns the points that fell off the
    /// bottom of the stack; the caller prunes them.
    pub fn push_irreversible(&mut self, entry: UndoEntry) -> Vec<OpId> {
        debug_assert!(matches!(entry.reversible, Reversible::No(_)));
        self.push(entry)
    }

    pub(crate) fn push(&mut self, entry: UndoEntry) -> Vec<OpId> {
        self.entries.push_back(entry);
        let mut pruned = Vec::new();
        while self.entries.len() > MAX_POINTS {
            if let Some(old) = self.entries.pop_front() {
                pruned.push(old.point.op);
            }
        }
        pruned
    }

    /// What an undo would hit, ignoring disk.
    pub fn top(&self) -> Result<&UndoEntry, Refusal> {
        let entry = self.entries.back().ok_or(Refusal::NothingToUndo)?;
        match entry.reversible {
            Reversible::Yes => Ok(entry),
            Reversible::No(why) => Err(Refusal::NotUndoable(why)),
        }
    }
}

/// Decides whether the top entry can be undone against `disk`, and builds
/// the plan. Refuses unless every recorded ref, the head's target and the
/// index are exactly where the operation left them (law 9: only the
/// recorded refs are ever touched).
pub fn plan(undo: &UndoStack, disk: &DiskState, op: OpId) -> Result<UndoReplay, Refusal> {
    let entry = undo.top()?;
    if disk.phase != Phase::Clean {
        return Err(Refusal::RepoBusy);
    }
    if HeadTarget::of(&disk.head) != entry.point.head {
        return Err(Refusal::Changed(UndoMoved::Head));
    }
    let mut refs = Vec::with_capacity(entry.point.refs.len());
    for (before, after) in entry.point.refs.iter().zip(&entry.after_refs) {
        let now = current_value(disk, &before.name);
        if now != after.oid {
            return Err(Refusal::Changed(UndoMoved::Ref(before.name.clone())));
        }
        refs.push(RestoreRef {
            name: before.name.clone(),
            expect: now,
            restore: before.oid.clone(),
        });
    }
    if disk.index != entry.after_index {
        return Err(Refusal::Changed(UndoMoved::Index));
    }
    Ok(UndoReplay {
        op,
        refs,
        tree: entry.tree.clone(),
        expect: Expect {
            head: disk.head.clone(),
            index: disk.index.clone(),
        },
    })
}

/// The value a recorded ref has on disk. `HEAD` is the detached head.
pub fn current_value(disk: &DiskState, name: &RefName) -> Option<Oid> {
    if name.0 == "HEAD" {
        return match &disk.head {
            Head::Detached(oid) => Some(oid.clone()),
            Head::Branch { .. } | Head::Unborn(_) => None,
        };
    }
    disk.refs.refs.get(name).cloned()
}
