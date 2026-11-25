//! Task 20: stash in the core. Requests name a stash by commit id and are
//! planned against the fresh snapshot: a stash that is not in the list is
//! refused and nothing is written (rule 5, law 12); a drop of the newest
//! stash can be undone, everything else says why it cannot (law 8).

use invoke_core::stash::stash_ref;
use invoke_core::*;
use proptest::collection::vec;
use proptest::prelude::*;

fn oid(n: u64) -> Oid {
    Oid::from_hex(&format!("{n:040x}")).unwrap()
}

fn entry(n: u64) -> StashEntry {
    StashEntry {
        id: oid(0xa000 + n),
        message: format!("On main: stash {n}"),
    }
}

fn disk(stashes: &[u64], phase: Phase, lock: LockState) -> DiskState {
    let mut refs = std::collections::BTreeMap::new();
    refs.insert(BranchName("main".into()).to_ref(), oid(1));
    if let Some(top) = stashes.first() {
        refs.insert(stash_ref(), entry(*top).id);
    }
    DiskState {
        head: Head::Branch {
            name: BranchName("main".into()),
            oid: oid(1),
        },
        phase,
        index: IndexId::Checksum(oid(100)),
        status: StatusSnapshot::default(),
        refs: RefsSnapshot {
            refs,
            ..RefsSnapshot::default()
        },
        stashes: stashes.iter().map(|n| entry(*n)).collect(),
        lock,
        advanced: Default::default(),
    }
}

/// An opened repo whose first read has landed.
fn opened(disk: DiskState) -> RepoState {
    let (state, commands) = open(RepoId {
        root_commit: None,
        remote_url: None,
    });
    let Command::ReadState { read, .. } = commands[0] else {
        panic!("open must read first");
    };
    step(state, Event::StateRead { read, disk }).0
}

fn writes(commands: &[Command]) -> Vec<&Command> {
    commands.iter().filter(|c| c.is_write()).collect()
}

fn refusal(state: &RepoState) -> Option<(Action, Refusal)> {
    match &state.notice {
        Some(Notice::Refused { action, reason }) => Some((*action, reason.clone())),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Apply,
    Pop,
    Drop,
}

fn request(kind: Kind, id: Oid) -> Event {
    match kind {
        Kind::Apply => Event::StashApplyRequested(id),
        Kind::Pop => Event::StashPopRequested(id),
        Kind::Drop => Event::StashDropRequested(id),
    }
}

fn kinds() -> impl Strategy<Value = Kind> {
    prop_oneof![Just(Kind::Apply), Just(Kind::Pop), Just(Kind::Drop)]
}

/// Distinct stash numbers, newest first.
fn stash_list() -> impl Strategy<Value = Vec<u64>> {
    proptest::sample::subsequence((0..8u64).collect::<Vec<_>>(), 0..5).prop_shuffle()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The entry is found by id: its position goes to git, and the run
    /// carries the id so the shell can check it (rule 5). An id that is not
    /// in the list is refused with that id named, and nothing is written.
    #[test]
    fn stashes_are_found_by_id_or_refused(
        list in stash_list(),
        probe in 0..8u64,
        kind in kinds(),
    ) {
        let state = opened(disk(&list, Phase::Clean, LockState::Free));
        let id = entry(probe).id;
        let (after, commands) = step(state, request(kind, id.clone()));
        match list.iter().position(|n| *n == probe) {
            Some(index) => {
                prop_assert_eq!(refusal(&after), None);
                let args = commands.iter().find_map(|c| match c {
                    Command::RunGit { args, .. } => Some(args.clone()),
                    _ => None,
                });
                let expected = match kind {
                    Kind::Apply => GitArgs::StashApply { index, stash: id },
                    Kind::Pop => GitArgs::StashPop { index, stash: id },
                    Kind::Drop => GitArgs::StashDrop { index, stash: id },
                };
                prop_assert_eq!(args, Some(expected));
            }
            None => {
                let action = match kind {
                    Kind::Apply => Action::StashApply,
                    Kind::Pop => Action::StashPop,
                    Kind::Drop => Action::StashDrop,
                };
                prop_assert_eq!(refusal(&after), Some((action, Refusal::StashGone(id))));
                prop_assert!(writes(&commands).is_empty());
                prop_assert!(after.ops.running.is_none() && after.ops.queued.is_empty());
            }
        }
    }

    /// A stash that was in the list when the user clicked but is gone when
    /// the request is dispatched (a read landed in between) is refused: the
    /// plan always uses the snapshot the write runs on.
    #[test]
    fn a_stash_that_moved_before_dispatch_is_refused(
        list in stash_list().prop_filter("needs one", |l| !l.is_empty()),
        kind in kinds(),
    ) {
        let first = disk(&list, Phase::Clean, LockState::Free);
        let mut state = opened(first.clone());
        // The lock is held by someone else, so the request waits.
        let locked = DiskState { lock: LockState::HeldByOther, ..first.clone() };
        let (s, commands) = step(state, Event::RefreshRequested);
        let Command::ReadState { read, .. } = commands[0] else { panic!() };
        let (s, _) = step(s, Event::StateRead { read, disk: locked });
        let (s, commands) = step(s, request(kind, entry(list[0]).id));
        prop_assert!(writes(&commands).is_empty());
        prop_assert_eq!(s.ops.queued.len(), 1);
        // Meanwhile the stash is dropped by someone else; the lock frees.
        let (s, commands) = step(s, Event::RefreshRequested);
        let Command::ReadState { read, .. } = commands[0] else { panic!() };
        let gone = disk(&list[1..], Phase::Clean, LockState::Free);
        let (s, commands) = step(s, Event::StateRead { read, disk: gone });
        prop_assert!(writes(&commands).is_empty(), "{commands:?}");
        prop_assert_eq!(
            refusal(&s).map(|(_, reason)| reason),
            Some(Refusal::StashGone(entry(list[0]).id))
        );
        state = s;
        prop_assert!(state.ops.queued.is_empty());
    }

    /// Law 12: a stash write waits for a fresh snapshot and a free lock.
    #[test]
    fn stash_writes_wait_for_a_fresh_unlocked_snapshot(
        list in stash_list().prop_filter("needs one", |l| !l.is_empty()),
        kind in kinds(),
        held_by_us in any::<bool>(),
    ) {
        let lock = if held_by_us { LockState::HeldByUs } else { LockState::HeldByOther };
        let state = opened(disk(&list, Phase::Clean, lock));
        let (after, commands) = step(state, request(kind, entry(list[0]).id));
        prop_assert!(commands.is_empty(), "{commands:?}");
        prop_assert_eq!(after.ops.queued.len(), 1);

        // A pending read counts as stale too.
        let state = opened(disk(&list, Phase::Clean, LockState::Free));
        let (state, _) = step(state, Event::RefreshRequested);
        let (_, commands) = step(state, Event::StashSaveRequested {
            message: String::new(),
            include_untracked: true,
        });
        prop_assert!(commands.is_empty(), "{commands:?}");
    }

    /// Save, apply and pop are refused while git is in the middle of a merge,
    /// rebase, cherry-pick or revert; a drop touches only the stash list and
    /// is not.
    #[test]
    fn busy_repos_refuse_what_touches_the_worktree(
        list in stash_list().prop_filter("needs one", |l| !l.is_empty()),
        which in 0..4usize,
    ) {
        let conflicts = Conflicts::default();
        let phase = [
            Phase::Merging { conflicts: conflicts.clone() },
            Phase::Rebasing { step: 1, total: 2, conflicts: conflicts.clone(), pause: Default::default(), origin: None },
            Phase::CherryPicking { conflicts: conflicts.clone() },
            Phase::Reverting { conflicts },
        ][which].clone();
        let id = entry(list[0]).id;
        let events = [
            (Event::StashSaveRequested { message: "x".into(), include_untracked: false }, Action::StashSave),
            (Event::StashApplyRequested(id.clone()), Action::StashApply),
            (Event::StashPopRequested(id.clone()), Action::StashPop),
        ];
        for (event, action) in events {
            let state = opened(disk(&list, phase.clone(), LockState::Free));
            let (after, commands) = step(state, event);
            prop_assert_eq!(refusal(&after), Some((action, Refusal::RepoBusy)));
            prop_assert!(writes(&commands).is_empty());
        }
        let state = opened(disk(&list, phase, LockState::Free));
        let (_, commands) = step(state, Event::StashDropRequested(id));
        prop_assert!(!writes(&commands).is_empty());
    }

    /// A save carries one clean line of message, whatever the user typed.
    #[test]
    fn save_messages_reach_git_as_one_clean_line(
        message in "[a-z \\n\\r\\t\\x00-]{0,20}",
        include_untracked in any::<bool>(),
    ) {
        let state = opened(disk(&[], Phase::Clean, LockState::Free));
        let (_, commands) = step(state, Event::StashSaveRequested {
            message: message.clone(),
            include_untracked,
        });
        let Some(Command::RunGit { args: GitArgs::StashSave { message: sent, include_untracked: u }, .. }) =
            commands.iter().find(|c| matches!(c, Command::RunGit { .. }))
        else {
            return Err(TestCaseError::fail(format!("no save run: {commands:?}")));
        };
        prop_assert_eq!(*u, include_untracked);
        prop_assert!(!sent.contains(['\n', '\r', '\0']));
        prop_assert_eq!(sent.trim(), sent.as_str());
        prop_assert_eq!(sent, &message.split(['\n', '\r']).next().unwrap_or("").replace('\0', "").trim().to_owned());
    }
}

/// Runs one stash request to the end: dispatch, the shell's snapshot, the
/// outcome and the read that follows. `after` is the disk the operation left.
fn finish(state: RepoState, event: Event, outcome: OpOutcome, after: DiskState) -> RepoState {
    let (state, commands) = step(state, event);
    let Some(Command::SnapshotForUndo(point)) = commands.first() else {
        panic!("no snapshot: {commands:?}");
    };
    let op = point.op;
    let (state, _) = step(state, Event::UndoSnapshotTaken { op, tree: oid(7) });
    let (state, commands) = step(state, Event::OpFinished(op, outcome));
    let Some(Command::ReadState { read, .. }) = commands
        .iter()
        .find(|c| matches!(c, Command::ReadState { .. }))
    else {
        panic!("no re-read");
    };
    step(
        state,
        Event::StateRead {
            read: *read,
            disk: after,
        },
    )
    .0
}

fn undo(state: RepoState) -> (RepoState, Vec<Command>) {
    step(state, Event::UndoRequested)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Law 8 for a drop: the newest stash can be put back, and only if the
    /// stash list is exactly as the drop left it. Branch switches, staging
    /// and merges in progress are no obstacle (the drop touched none).
    #[test]
    fn dropping_the_newest_stash_can_be_undone(
        rest in stash_list().prop_filter("distinct from 9", |l| !l.contains(&9)),
        moved in any::<bool>(),
        other_changes in any::<bool>(),
    ) {
        let mut before = vec![9];
        before.extend(&rest);
        let state = opened(disk(&before, Phase::Clean, LockState::Free));
        let state = finish(
            state,
            Event::StashDropRequested(entry(9).id),
            OpOutcome::Succeeded,
            disk(&rest, Phase::Clean, LockState::Free),
        );
        prop_assert_eq!(state.undo.entries.len(), 1);
        prop_assert_eq!(state.undo.entries[0].reversible, Reversible::Yes);

        // Someone else saved a stash (the list moved), or only other
        // things changed.
        let mut now = disk(&rest, Phase::Clean, LockState::Free);
        if moved {
            let mut list = vec![8];
            list.extend(&rest);
            now = disk(&list, Phase::Clean, LockState::Free);
        }
        if other_changes {
            now.index = IndexId::Checksum(oid(101));
            now.head = Head::Branch { name: BranchName("dev".into()), oid: oid(2) };
            now.phase = Phase::Merging { conflicts: Conflicts::default() };
        }
        let (state, commands) = step(state, Event::DiskChanged(ChangeSet(
            [Area::Refs].into_iter().collect(),
        )));
        let Command::ReadState { read, .. } = commands[0] else { panic!() };
        let (state, _) = step(state, Event::StateRead { read, disk: now });
        let (after, commands) = undo(state);
        if moved {
            prop_assert_eq!(
                refusal(&after),
                Some((Action::Undo, Refusal::Changed(UndoMoved::Ref(stash_ref()))))
            );
            prop_assert!(writes(&commands).is_empty());
        } else {
            prop_assert_eq!(refusal(&after), None);
            let replay = commands.iter().find_map(|c| match c {
                Command::ReplayUndo(replay) => Some(replay.clone()),
                _ => None,
            });
            let replay = replay.expect("a replay");
            // Law 9: no ref is restored by the replay; only the stash is put back.
            prop_assert!(replay.refs.is_empty());
            prop_assert_eq!(
                replay.stash,
                Some(StashRestore { entry: entry(9), expect_top: rest.first().map(|n| entry(*n).id) })
            );
        }
    }

    /// A drop of an older stash, and a save, apply or pop, are on the stack
    /// as "not undoable" with the reason, so an undo stops at them instead
    /// of reversing an older operation behind them.
    #[test]
    fn everything_else_says_why_it_cannot_be_undone(
        newer in 1..3usize,
        which in 0..4usize,
    ) {
        let list: Vec<u64> = (0..=newer as u64).collect();
        let last = *list.last().unwrap();
        let (event, why) = match which {
            0 => (Event::StashDropRequested(entry(last).id), Irreversible::StashNotOnTop),
            1 => (Event::StashPopRequested(entry(0).id), Irreversible::StashRewroteFiles),
            2 => (Event::StashApplyRequested(entry(0).id), Irreversible::StashRewroteFiles),
            _ => (
                Event::StashSaveRequested { message: "m".into(), include_untracked: true },
                Irreversible::StashRewroteFiles,
            ),
        };
        let state = opened(disk(&list, Phase::Clean, LockState::Free));
        let state = finish(
            state,
            event,
            OpOutcome::Succeeded,
            disk(&list[1..], Phase::Clean, LockState::Free),
        );
        prop_assert_eq!(state.undo.entries.len(), 1);
        prop_assert_eq!(state.undo.entries[0].reversible, Reversible::No(why));
        let (after, commands) = undo(state);
        prop_assert_eq!(refusal(&after), Some((Action::Undo, Refusal::NotUndoable(why))));
        prop_assert!(commands.is_empty());
    }

    /// A pop that conflicts is an expected outcome, not an error: the stash
    /// is still in the (re-read) list, no undo point is kept, and the core
    /// never patches the snapshot itself.
    #[test]
    fn a_conflicted_pop_keeps_the_stash_and_records_nothing(
        list in stash_list().prop_filter("needs one", |l| !l.is_empty()),
    ) {
        let state = opened(disk(&list, Phase::Clean, LockState::Free));
        let expected = Failure { class: ErrorClass::Expected, details: "CONFLICT".into() };
        let (state, commands) = step(state, Event::StashPopRequested(entry(list[0]).id));
        let Some(Command::SnapshotForUndo(point)) = commands.first() else { panic!() };
        let op = point.op;
        let (state, _) = step(state, Event::UndoSnapshotTaken { op, tree: oid(7) });
        let snapshot_before = state.snapshot.clone();
        let (state, commands) = step(state, Event::OpFinished(op, OpOutcome::Failed(expected.clone())));
        prop_assert_eq!(&state.snapshot, &snapshot_before);
        prop_assert!(state.undo.entries.is_empty());
        prop_assert_eq!(state.undo.pending.clone(), UndoPending::Idle);
        prop_assert!(commands.contains(&Command::PruneUndo(vec![op])));
        prop_assert_eq!(
            state.notice.clone(),
            Some(Notice::OpEnded { op, outcome: OpOutcome::Failed(expected) })
        );
        // The re-read shows git's own state: the stash is still there.
        let Some(Command::ReadState { read, .. }) = commands.iter().find(|c| matches!(c, Command::ReadState { .. })) else { panic!() };
        let (state, _) = step(state, Event::StateRead {
            read: *read,
            disk: disk(&list, Phase::Clean, LockState::Free),
        });
        prop_assert_eq!(state.fresh_disk().unwrap().stashes.len(), list.len());
    }

    /// Law 13: a running stash op is not killed. `git stash` saves the
    /// stash and then resets the worktree, so a kill could leave both.
    #[test]
    fn a_running_stash_op_is_not_cancelled(
        list in stash_list().prop_filter("needs one", |l| !l.is_empty()),
        which in 0..4usize,
    ) {
        let id = entry(list[0]).id;
        let event = match which {
            0 => Event::StashApplyRequested(id),
            1 => Event::StashPopRequested(id),
            2 => Event::StashDropRequested(id),
            _ => Event::StashSaveRequested { message: String::new(), include_untracked: false },
        };
        let state = opened(disk(&list, Phase::Clean, LockState::Free));
        let (state, _) = step(state, event);
        let running = state.ops.running.clone().expect("running");
        let (after, commands) = step(state.clone(), Event::OpCancelRequested(running.id));
        prop_assert!(!commands.iter().any(|c| matches!(c, Command::CancelOp(_))));
        prop_assert_eq!(after.ops.running, Some(running));
    }

    /// Commits still cancel, and queued stash ops can be cancelled before
    /// they start.
    #[test]
    fn queued_stash_ops_can_be_cancelled(n in 1..4usize) {
        let state = opened(disk(&[0], Phase::Clean, LockState::HeldByOther));
        let mut state = state;
        for _ in 0..n {
            state = step(state, Event::StashSaveRequested {
                message: String::new(),
                include_untracked: false,
            }).0;
        }
        let id = state.ops.queued[0].id;
        let (state, _) = step(state, Event::OpCancelRequested(id));
        prop_assert_eq!(state.ops.queued.len(), n - 1);
    }
}

proptest! {
    /// `step` never panics on stash events in any order.
    #[test]
    fn stash_events_never_panic(
        events in vec((0..5usize, 0..4u64, any::<bool>()), 0..30),
        list in stash_list(),
    ) {
        let mut state = opened(disk(&list, Phase::Clean, LockState::Free));
        for (which, n, flag) in events {
            let event = match which {
                0 => Event::StashSaveRequested { message: "m".into(), include_untracked: flag },
                1 => Event::StashApplyRequested(entry(n).id),
                2 => Event::StashPopRequested(entry(n).id),
                3 => Event::StashDropRequested(entry(n).id),
                _ => Event::UndoRequested,
            };
            state = step(state, event).0;
        }
    }
}
