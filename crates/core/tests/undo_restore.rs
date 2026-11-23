//! Undo points kept from an earlier run (task 22, part 2): when the core takes
//! them and what it does with op ids.

use invoke_core::{
    Command, Event, HeadTarget, IndexEffect, IndexId, MAX_POINTS, Oid, OpId, RepoId, Reversal,
    Reversible, UndoEntry, UndoPending, UndoPoint, open, step,
};
use proptest::prelude::*;

fn oid(c: char) -> Oid {
    Oid::from_hex(&c.to_string().repeat(40)).unwrap()
}

fn entry(op: u64) -> UndoEntry {
    UndoEntry {
        point: UndoPoint {
            op: OpId(op),
            label: format!("op {op}"),
            head: HeadTarget::Detached,
            refs: Vec::new(),
            index: IndexId::Absent,
            index_effect: IndexEffect::Untouched,
            reverse: Reversal::Refs,
        },
        tree: oid('a'),
        after_refs: Vec::new(),
        after_index: IndexId::Absent,
        after_head: HeadTarget::Detached,
        reversible: Reversible::Yes,
    }
}

fn fresh() -> invoke_core::RepoState {
    open(RepoId {
        root_commit: None,
        remote_url: None,
    })
    .0
}

#[test]
fn kept_points_come_back_in_order_and_new_ops_do_not_reuse_their_ids() {
    let state = fresh();
    assert!(state.ops.next.0 <= 5);
    let (state, commands) = step(
        state,
        Event::UndoStackRestored(vec![entry(3), entry(9), entry(4)]),
    );
    assert!(commands.is_empty());
    let ops: Vec<u64> = state.undo.entries.iter().map(|e| e.point.op.0).collect();
    assert_eq!(ops, vec![3, 9, 4]);
    assert_eq!(state.undo.top().unwrap().point.op, OpId(4));
    // The saved trees are named by op id: the next op starts past the highest.
    assert_eq!(state.ops.next, OpId(10));
}

#[test]
fn nothing_is_taken_once_something_was_recorded() {
    let (state, _) = step(fresh(), Event::UndoStackRestored(vec![entry(1)]));
    let (state, commands) = step(state, Event::UndoStackRestored(vec![entry(2), entry(3)]));
    assert!(commands.is_empty());
    assert_eq!(state.undo.entries.len(), 1);
    assert_eq!(state.undo.entries[0].point.op, OpId(1));

    // An operation in flight also means "not now".
    let mut busy = fresh();
    busy.undo.pending = UndoPending::Rewinding;
    let (busy, _) = step(busy, Event::UndoStackRestored(vec![entry(1)]));
    assert!(busy.undo.entries.is_empty());
}

#[test]
fn only_the_newest_points_are_kept_and_the_rest_are_pruned() {
    let all: Vec<UndoEntry> = (1..=(MAX_POINTS as u64 + 3)).map(entry).collect();
    let (state, commands) = step(fresh(), Event::UndoStackRestored(all));
    assert_eq!(state.undo.entries.len(), MAX_POINTS);
    assert_eq!(state.undo.entries[0].point.op, OpId(4));
    assert_eq!(
        commands,
        vec![Command::PruneUndo(vec![OpId(1), OpId(2), OpId(3)])]
    );
}

#[test]
fn an_empty_restore_changes_nothing() {
    let before = fresh();
    let next = before.ops.next;
    let (state, commands) = step(before, Event::UndoStackRestored(Vec::new()));
    assert!(commands.is_empty() && state.undo.entries.is_empty());
    assert_eq!(state.ops.next, next);
}

proptest! {
    /// Any list of points: the stack keeps the newest ones, in order, the
    /// others are named for pruning exactly once, and the next op id is past
    /// every id on the stack.
    #[test]
    fn restoring_keeps_the_newest_and_never_reuses_an_id(
        ops in proptest::collection::vec(1u64..1_000, 0..120),
    ) {
        let entries: Vec<UndoEntry> = ops.iter().copied().map(entry).collect();
        let (state, commands) = step(fresh(), Event::UndoStackRestored(entries));
        let keep = ops.len().min(MAX_POINTS);
        let kept: Vec<u64> = state.undo.entries.iter().map(|e| e.point.op.0).collect();
        prop_assert_eq!(&kept[..], &ops[ops.len() - keep..]);
        let dropped: Vec<u64> = commands
            .iter()
            .flat_map(|c| match c {
                Command::PruneUndo(ops) => ops.iter().map(|o| o.0).collect(),
                _ => Vec::new(),
            })
            .collect();
        prop_assert_eq!(&dropped[..], &ops[..ops.len() - keep]);
        if let Some(max) = kept.iter().max() {
            prop_assert!(state.ops.next.0 > *max);
        }
    }
}
