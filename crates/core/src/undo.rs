//! The undo stack (law 8, law 9): what each write recorded before it ran,
//! what it left behind, and the plan for putting the old values back.
//!
//! Everything here is pure. The shell takes the snapshots and replays the
//! plans (`docs/spec/core-design.md` -> "Undo design").

use std::collections::VecDeque;

use crate::event::{Expect, HeadTarget, RefValue, UndoPoint};
use crate::ids::{IndexId, Oid, OpId, RefName};
use crate::stash::{self, StashEntry};
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
    /// A stash save, apply or pop rewrote working files. Undo never rewrites
    /// them (only a discard's undo does, task 13); the stash entry itself is
    /// the way back.
    StashRewroteFiles,
    /// A stash that was not the newest was dropped. Putting it back at its
    /// old position would need the reflog rewritten; it can only go on top,
    /// which is not an exact restore (law 8).
    StashNotOnTop,
}

/// How an operation is reversed (task 20; recorded in its [`UndoPoint`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reversal {
    /// Restore the recorded refs and the index tree (commit, amend).
    Refs,
    /// Put a dropped stash back with `git stash store`. The commit is kept
    /// alive under `refs/invoke-git/undo/<op>-stash` until the point goes.
    StashStore(StashEntry),
    /// A stash that was not the newest was dropped: not undoable (see
    /// [`Irreversible::StashNotOnTop`]), but the commit is kept alive the
    /// same way, so the work can be recovered by hand.
    StashKept(StashEntry),
    /// Not undoable; an undo stops here with this reason.
    Never(Irreversible),
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
    /// Task 20: when set, the replay puts this stash back (the refs and
    /// index above are then empty / unused: the stash is stored, not the
    /// refs restored). The shell checks the stash list still has
    /// `expect_top` on top first.
    pub stash: Option<StashRestore>,
}

/// Putting a dropped stash back on top of the stash list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StashRestore {
    pub entry: StashEntry,
    /// What the newest stash must be now (`None`: the list must be empty).
    pub expect_top: Option<Oid>,
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
    if let Reversal::StashStore(stashed) = &entry.point.reverse {
        return plan_stash_store(entry, stashed, disk, op);
    }
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
        stash: None,
    })
}

/// Undo of a stash drop touches only the stash list, so only that is
/// checked: the stash list must still be as the drop left it (nobody saved,
/// popped or dropped since). Branch switches, staging and merges in
/// progress do not matter to it.
fn plan_stash_store(
    entry: &UndoEntry,
    stashed: &StashEntry,
    disk: &DiskState,
    op: OpId,
) -> Result<UndoReplay, Refusal> {
    let name = stash::stash_ref();
    let after = entry.after_refs.iter().find(|r| r.name == name);
    let now = current_value(disk, &name);
    if now != after.and_then(|after| after.oid.clone()) {
        return Err(Refusal::Changed(UndoMoved::Ref(name)));
    }
    Ok(UndoReplay {
        op,
        refs: Vec::new(),
        tree: entry.tree.clone(),
        expect: Expect {
            head: disk.head.clone(),
            index: disk.index.clone(),
        },
        stash: Some(StashRestore {
            entry: stashed.clone(),
            expect_top: now,
        }),
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
