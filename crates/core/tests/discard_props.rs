//! Backlog task 13, core half: a discard goes through the op queue (fresh
//! snapshot, free lock, clean phase, expected index id and file bytes, undo
//! point), and its undo writes the saved file back without touching refs or
//! the index.

use invoke_core::*;
use proptest::prelude::*;

fn hex(c: char) -> Oid {
    Oid::from_hex(&c.to_string().repeat(40)).unwrap()
}

fn index(c: char) -> IndexId {
    IndexId::Checksum(hex(c))
}

fn disk(index: IndexId, lock: LockState, phase: Phase) -> DiskState {
    DiskState {
        head: Head::Branch {
            name: BranchName("main".into()),
            oid: hex('a'),
        },
        phase,
        index,
        status: StatusSnapshot::default(),
        refs: RefsSnapshot {
            refs: [(RefName("refs/heads/main".into()), hex('a'))].into(),
            ..RefsSnapshot::default()
        },
        stashes: Vec::new(),
        lock,
    }
}

fn clean(index: IndexId) -> DiskState {
    disk(index, LockState::Free, Phase::Clean)
}

fn request(path: &str, based_on: IndexId, file: Option<Oid>) -> DiscardRequest {
    DiscardRequest {
        selection: Selection::Lines {
            path: RepoPath(path.into()),
            hunk: 0,
            lines: vec![1, 2, 3],
        },
        patch: Patch("diff --git a/f b/f\n".into()),
        based_on,
        expect_file: file,
    }
}

fn ready(disk: DiskState) -> RepoState {
    let repo_id = RepoId {
        root_commit: None,
        remote_url: None,
    };
    let (state, commands) = open(repo_id);
    let Command::ReadState { read, .. } = commands[0].clone() else {
        panic!("{commands:?}");
    };
    step(state, Event::StateRead { read, disk }).0
}

/// Delivers a fresh read of `disk` after a refresh.
fn read_again(state: RepoState, disk: DiskState) -> (RepoState, Vec<Command>) {
    let (state, commands) = step(state, Event::RefreshRequested);
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    step(state, Event::StateRead { read: *read, disk })
}

/// Runs a discard of `a.rs` to a settled undo point.
fn discarded(index_after: IndexId) -> RepoState {
    let state = ready(clean(index('b')));
    let (state, _) = step(
        state,
        Event::DiscardRequested(request("a.rs", index('b'), Some(hex('e')))),
    );
    let op = state.ops.running.as_ref().unwrap().id;
    let (state, _) = step(state, Event::UndoSnapshotTaken { op, tree: hex('c') });
    let (state, commands) = step(state, Event::OpFinished(op, OpOutcome::Succeeded));
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    step(
        state,
        Event::StateRead {
            read: *read,
            disk: clean(index_after),
        },
    )
    .0
}

#[test]
fn discard_snapshots_then_applies_with_the_expected_index_and_file() {
    let state = ready(clean(index('b')));
    let (state, commands) = step(
        state,
        Event::DiscardRequested(request("src/a.rs", index('b'), Some(hex('e')))),
    );
    let op = state.ops.running.as_ref().expect("running").id;
    assert_eq!(
        commands,
        vec![
            Command::SnapshotForUndo(UndoPoint {
                op,
                label: "Discard 3 lines in src/a.rs".into(),
                head: HeadTarget::Branch(BranchName("main".into())),
                refs: Vec::new(),
                index: index('b'),
                index_effect: IndexEffect::Untouched,
                reverse: Reversal::Discard {
                    path: RepoPath("src/a.rs".into())
                },
            }),
            Command::ApplyPatchToWorktree {
                op,
                patch: Patch("diff --git a/f b/f\n".into()),
                path: RepoPath("src/a.rs".into()),
                expect_index: index('b'),
                expect_file: Some(hex('e')),
            },
        ]
    );
    assert!(commands.iter().all(Command::is_write));
}

#[test]
fn the_label_names_what_was_selected() {
    let state = ready(clean(index('b')));
    let mut req = request("a.rs", index('b'), None);
    req.selection = Selection::File(RepoPath("a.rs".into()));
    let (_, commands) = step(state, Event::DiscardRequested(req));
    let Command::SnapshotForUndo(point) = &commands[0] else {
        panic!("{commands:?}");
    };
    assert_eq!(point.label, "Discard a.rs");
}

#[test]
fn a_patch_from_an_older_index_is_refused_and_nothing_runs() {
    let state = ready(clean(index('b')));
    let (state, commands) = step(
        state,
        Event::DiscardRequested(request("a", index('c'), None)),
    );
    assert!(commands.is_empty(), "{commands:?}");
    assert!(state.ops.running.is_none() && state.ops.queued.is_empty());
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Discard,
            reason: Refusal::StaleDiff
        })
    );
}

#[test]
fn an_empty_patch_is_refused_at_once() {
    let state = ready(clean(index('b')));
    let mut req = request("a", index('b'), None);
    req.patch = Patch(String::new());
    let (state, commands) = step(state, Event::DiscardRequested(req));
    assert!(commands.is_empty());
    assert!(state.ops.queued.is_empty() && state.ops.running.is_none());
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Discard,
            reason: Refusal::NothingSelected
        })
    );
}

#[test]
fn a_merge_rebase_cherry_pick_or_revert_refuses_a_discard() {
    let phases = [
        Phase::Merging {
            conflicts: Conflicts::default(),
        },
        Phase::Rebasing {
            step: 1,
            total: 2,
            conflicts: Conflicts::default(),
            pause: Default::default(),
            origin: None,
        },
        Phase::CherryPicking {
            conflicts: Conflicts::default(),
        },
        Phase::Reverting {
            conflicts: Conflicts::default(),
        },
    ];
    for phase in phases {
        let state = ready(disk(index('b'), LockState::Free, phase.clone()));
        let (state, commands) = step(
            state,
            Event::DiscardRequested(request("a", index('b'), None)),
        );
        assert!(commands.is_empty(), "{phase:?}: {commands:?}");
        assert_eq!(
            state.notice,
            Some(Notice::Refused {
                action: Action::Discard,
                reason: Refusal::RepoBusy
            }),
            "{phase:?}"
        );
    }
}

#[test]
fn discard_waits_for_the_lock_then_runs_on_the_fresh_read() {
    let state = ready(disk(index('b'), LockState::HeldByOther, Phase::Clean));
    let (state, commands) = step(
        state,
        Event::DiscardRequested(request("a", index('b'), None)),
    );
    assert!(commands.is_empty());
    assert_eq!(state.ops.queued.len(), 1);
    let (_, commands) = read_again(state, clean(index('b')));
    assert!(matches!(
        commands.as_slice(),
        [
            Command::SnapshotForUndo(_),
            Command::ApplyPatchToWorktree { .. }
        ]
    ));
}

#[test]
fn a_queued_discard_checks_the_index_it_will_really_run_on() {
    let state = ready(disk(index('b'), LockState::HeldByOther, Phase::Clean));
    let (state, _) = step(
        state,
        Event::DiscardRequested(request("a", index('b'), None)),
    );
    let (state, commands) = read_again(state, clean(index('c')));
    assert!(commands.is_empty());
    assert!(state.ops.queued.is_empty() && state.ops.running.is_none());
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Discard,
            reason: Refusal::StaleDiff
        })
    );
}

#[test]
fn a_finished_discard_can_be_undone_and_the_plan_touches_only_the_file() {
    // The index does not change under a discard.
    let state = discarded(index('b'));
    let entry = state.undo.entries.back().expect("an undo point");
    assert_eq!(entry.point.label, "Discard 3 lines in a.rs");
    assert_eq!(entry.point.index_effect, IndexEffect::Untouched);
    let (_, commands) = step(state, Event::UndoRequested);
    let [Command::ReplayUndo(replay)] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    assert_eq!(
        replay.discard,
        Some(DiscardRestore {
            discarded: OpId(0),
            path: RepoPath("a.rs".into())
        })
    );
    assert!(replay.refs.is_empty() && replay.head.is_none() && replay.stash.is_none());
    assert_eq!(replay.index_effect, IndexEffect::Untouched);
}

#[test]
fn undo_of_a_discard_does_not_care_where_head_or_the_index_went() {
    // The bytes of the file decide (the shell checks them), not refs.
    let state = discarded(index('b'));
    let mut elsewhere = clean(index('d'));
    elsewhere.head = Head::Detached(hex('f'));
    let (state, _) = read_again(state, elsewhere);
    let (_, commands) = step(state, Event::UndoRequested);
    assert!(
        matches!(commands.as_slice(), [Command::ReplayUndo(r)] if r.discard.is_some()),
        "{commands:?}"
    );
}

#[test]
fn undo_of_a_discard_is_refused_in_a_merge() {
    let state = discarded(index('b'));
    let merging = disk(
        index('b'),
        LockState::Free,
        Phase::Merging {
            conflicts: Conflicts::default(),
        },
    );
    let (state, _) = read_again(state, merging);
    let (state, commands) = step(state, Event::UndoRequested);
    assert!(commands.is_empty(), "{commands:?}");
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Undo,
            reason: Refusal::RepoBusy
        })
    );
}

#[test]
fn a_failed_discard_leaves_no_undo_point() {
    let state = ready(clean(index('b')));
    let (state, _) = step(
        state,
        Event::DiscardRequested(request("a.rs", index('b'), None)),
    );
    let op = state.ops.running.as_ref().unwrap().id;
    let (state, _) = step(state, Event::UndoSnapshotTaken { op, tree: hex('c') });
    let failure = Failure {
        class: ErrorClass::PreconditionChanged,
        details: "the file changed".into(),
    };
    let (state, commands) = step(state, Event::OpFinished(op, OpOutcome::Failed(failure)));
    assert!(
        commands.contains(&Command::PruneUndo(vec![op])),
        "{commands:?}"
    );
    assert!(state.undo.entries.is_empty());
}

#[test]
fn a_running_discard_is_not_cancelled() {
    let state = ready(clean(index('b')));
    let (state, _) = step(
        state,
        Event::DiscardRequested(request("a.rs", index('b'), None)),
    );
    let op = state.ops.running.as_ref().unwrap().id;
    let (state, commands) = step(state, Event::OpCancelRequested(op));
    assert!(
        !commands.contains(&Command::CancelOp(op)),
        "a discard is not killed halfway: {commands:?}"
    );
    assert!(state.ops.running.is_some());
}

proptest! {
    /// Whatever the disk looks like, a discard runs exactly when its diff
    /// was made against the index on disk and the repo is not mid-merge,
    /// and then it expects that index and the file the request names.
    #[test]
    fn a_discard_runs_iff_its_diff_matches_the_index(
        disk_index in prop_oneof![Just(IndexId::Absent), "[a-f]".prop_map(|c| index(c.chars().next().unwrap()))],
        based_on in prop_oneof![Just(IndexId::Absent), "[a-f]".prop_map(|c| index(c.chars().next().unwrap()))],
        file in proptest::option::of("[a-f]".prop_map(|c| hex(c.chars().next().unwrap()))),
        merging in any::<bool>(),
    ) {
        let phase = if merging {
            Phase::Merging { conflicts: Conflicts::default() }
        } else {
            Phase::Clean
        };
        let state = ready(disk(disk_index.clone(), LockState::Free, phase));
        let (state, commands) = step(
            state,
            Event::DiscardRequested(request("a", based_on.clone(), file.clone())),
        );
        if based_on == disk_index && !merging {
            let applied = commands.iter().any(|c| matches!(
                c,
                Command::ApplyPatchToWorktree { expect_index, expect_file, .. }
                    if *expect_index == disk_index && *expect_file == file
            ));
            prop_assert!(applied, "{:?}", commands);
        } else {
            prop_assert!(commands.is_empty());
            let reason = if merging { Refusal::RepoBusy } else { Refusal::StaleDiff };
            prop_assert_eq!(
                state.notice,
                Some(Notice::Refused { action: Action::Discard, reason })
            );
        }
    }
}
