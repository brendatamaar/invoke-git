use invoke_core::*;
use proptest::prelude::*;

fn oid(number: u8) -> Oid {
    Oid::from_hex(&format!("{number:040x}")).unwrap()
}

fn disk(number: u8) -> DiskState {
    DiskState {
        head: Head::Branch {
            name: BranchName("main".into()),
            oid: oid(number),
        },
        phase: Phase::Clean,
        index: IndexId::Checksum(oid(number)),
        status: StatusSnapshot::default(),
        refs: RefsSnapshot::default(),
        stashes: Vec::new(),
        lock: LockState::Free,
    }
}

fn read(state: RepoState, commands: &[Command], disk: DiskState) -> (RepoState, Vec<Command>) {
    let read = commands
        .iter()
        .find_map(|command| match command {
            Command::ReadState { read, .. } => Some(*read),
            _ => None,
        })
        .expect("fresh read");
    step(state, Event::StateRead { read, disk })
}

fn editing(number: u8) -> (RepoState, Vec<Command>) {
    let (state, commands) = open(RepoId {
        root_commit: None,
        remote_url: None,
    });
    let (state, _) = read(state, &commands, disk(number));
    let plan = RebasePlan::new(oid(0), vec![(oid(number), RebaseAction::Edit)]).unwrap();
    step(state, Event::RebasePlanEdited(plan))
}

proptest! {
    #[test]
    fn plans_validate_the_first_retained_action(
        actions in prop::collection::vec(0u8..5, 0..16),
    ) {
        let choices = [RebaseAction::Pick, RebaseAction::Squash, RebaseAction::Fixup, RebaseAction::Edit, RebaseAction::Drop];
        let rows = actions.iter().enumerate().map(|(i, &a)| (oid(i as u8 + 1), choices[a as usize])).collect();
        let plan = RebasePlan::new(oid(0), rows);
        let expected = !actions.is_empty() && !matches!(actions.iter().copied().find(|&a| a != 4), Some(1 | 2));
        prop_assert_eq!(plan.is_ok(), expected);
    }

    #[test]
    fn a_stale_plan_cannot_rewrite_a_moved_head(number in 1u8..254, moved in any::<bool>()) {
        let (state, edits) = editing(number);
        prop_assert!(edits.is_empty());
        let (state, refresh) = step(state, Event::RefreshRequested);
        let mut after = disk(number);
        if moved {
            after.head = disk(number + 1).head;
        }
        let (state, _) = read(state, &refresh, after);
        let (state, commands) = step(state, Event::RebaseStartRequested);
        if moved {
            prop_assert!(commands.is_empty());
            let refused = matches!(state.notice, Some(Notice::Refused { reason: Refusal::BranchChanged, .. }));
            prop_assert!(refused);
        } else {
            let guarded = matches!(commands.as_slice(), [Command::SnapshotForUndo(_), Command::RunGit { args: GitArgs::InteractiveRebase(_), expect, .. }] if expect.head == disk(number).head);
            prop_assert!(guarded);
        }
    }

    #[test]
    fn pause_keeps_the_original_undo_snapshot(number in 1u8..254, abort in any::<bool>()) {
        let (state, _) = editing(number);
        let (state, commands) = step(state, Event::RebaseStartRequested);
        let [Command::SnapshotForUndo(point), Command::RunGit { op, .. }] = commands.as_slice() else {
            panic!("expected snapshot and rebase");
        };
        let (state, cancelled) = step(state, Event::OpCancelRequested(*op));
        prop_assert!(cancelled.is_empty());
        let (state, _) = step(state, Event::UndoSnapshotTaken { op: *op, tree: oid(number) });
        let (state, commands) = step(state, Event::OpFinished(*op, OpOutcome::Succeeded));
        let mut paused = disk(number);
        paused.head = Head::Detached(oid(number + 1));
        paused.phase = Phase::Rebasing {
            step: 1,
            total: 1,
            conflicts: Conflicts::default(),
            pause: RebasePause::Edit,
            origin: Some(RebaseOrigin { branch: Some(BranchName("main".into())), tip: oid(number) }),
        };
        let (state, commands) = read(state, &commands, paused);
        prop_assert!(commands.is_empty());
        prop_assert!(matches!(state.rebase, RebaseState::Paused(_)));
        prop_assert!(state.undo.entries.is_empty());
        let next = if abort { Event::AbortRequested } else { Event::ContinueRequested };
        let (state, commands) = step(state, next);
        let [Command::RunGit { op, .. }] = commands.as_slice() else {
            panic!("the rebase already owns an undo snapshot");
        };
        let (state, commands) = step(state, Event::OpFinished(*op, OpOutcome::Succeeded));
        let after = if abort { disk(number) } else { disk(number + 1) };
        let (state, commands) = read(state, &commands, after);
        if abort {
            let pruned = matches!(commands.as_slice(), [Command::PruneUndo(ids)] if ids == &[point.op]);
            prop_assert!(pruned);
            prop_assert_eq!(state.rebase, RebaseState::Aborted);
            prop_assert!(state.undo.entries.is_empty());
        } else {
            prop_assert_eq!(state.rebase, RebaseState::Done);
            let recorded = state.undo.top().ok().map(|entry| &entry.point);
            prop_assert_eq!(recorded, Some(point));
        }
    }
}

#[test]
fn abort_clears_an_editing_or_locked_queued_plan_without_io() {
    let (state, _) = editing(1);
    let (state, commands) = step(state, Event::AbortRequested);
    assert_eq!(state.rebase, RebaseState::Aborted);
    assert!(commands.is_empty());

    let (state, _) = editing(1);
    let (state, refresh) = step(state, Event::RefreshRequested);
    let mut locked = disk(1);
    locked.lock = LockState::HeldByOther;
    let (state, _) = read(state, &refresh, locked);
    let (state, commands) = step(state, Event::RebaseStartRequested);
    assert!(commands.is_empty());
    let (state, commands) = step(state, Event::AbortRequested);
    assert_eq!(state.rebase, RebaseState::Aborted);
    assert!(state.ops.queued.is_empty());
    assert!(commands.is_empty());
}

#[test]
fn a_running_abort_waits_for_a_git_pause() {
    let (state, _) = editing(1);
    let (state, commands) = step(state, Event::RebaseStartRequested);
    let [Command::SnapshotForUndo(point), ..] = commands.as_slice() else {
        panic!("expected rebase snapshot");
    };
    let (state, commands) = step(state, Event::AbortRequested);
    assert!(commands.is_empty());
    let (state, _) = step(
        state,
        Event::UndoSnapshotTaken {
            op: point.op,
            tree: oid(1),
        },
    );
    let (state, commands) = step(state, Event::OpFinished(point.op, OpOutcome::Succeeded));
    let mut paused = disk(1);
    paused.head = Head::Detached(oid(2));
    paused.phase = Phase::Rebasing {
        step: 1,
        total: 1,
        pause: RebasePause::Edit,
        conflicts: Conflicts::default(),
        origin: Some(RebaseOrigin {
            branch: Some(BranchName("main".into())),
            tip: oid(1),
        }),
    };
    let (_, commands) = read(state, &commands, paused);
    assert!(matches!(
        commands.as_slice(),
        [Command::RunGit {
            args: GitArgs::GuardedRebase {
                control: rebase::RebaseControl::Abort,
                ..
            },
            ..
        }]
    ));
}
