//! Task 23 (core half): the conflict flow. Per-file Unresolved/Resolved,
//! Continue only when everything is Resolved, Abort always, rebase Skip,
//! and no write on a stale or locked snapshot (law 12), never touching a
//! branch the user did not target (law 9).

use invoke_core::*;
use proptest::collection::vec;
use proptest::prelude::*;

fn oid_of(c: char) -> Oid {
    Oid::from_hex(&c.to_string().repeat(40)).unwrap()
}

fn path_named(name: &str) -> RepoPath {
    RepoPath(name.into())
}

fn path() -> impl Strategy<Value = RepoPath> {
    "[a-z]{1,3}(/[a-z]{1,3})?".prop_map(RepoPath)
}

fn kind() -> impl Strategy<Value = ConflictKind> {
    prop_oneof![
        Just(ConflictKind::BothModified),
        Just(ConflictKind::BothAdded),
        Just(ConflictKind::DeletedByUs),
        Just(ConflictKind::DeletedByThem),
        Just(ConflictKind::AddedByUs),
        Just(ConflictKind::AddedByThem),
        Just(ConflictKind::BothDeleted),
    ]
}

fn conflicts() -> impl Strategy<Value = Conflicts> {
    (vec((path(), kind()), 0..4), vec(path(), 0..4))
        .prop_map(|(unresolved, resolved)| Conflicts::from_parts(unresolved, resolved))
}

/// Any paused phase.
fn paused() -> impl Strategy<Value = Phase> {
    prop_oneof![
        conflicts().prop_map(|conflicts| Phase::Merging { conflicts }),
        (0..6u32, 0..6u32, conflicts()).prop_map(|(step, total, conflicts)| Phase::Rebasing {
            step,
            total,
            conflicts,
            pause: Default::default(),
            origin: None
        }),
        conflicts().prop_map(|conflicts| Phase::CherryPicking { conflicts }),
        conflicts().prop_map(|conflicts| Phase::Reverting { conflicts }),
    ]
}

fn with_conflicts(phase: &Phase, conflicts: Conflicts) -> Phase {
    match phase {
        Phase::Clean => Phase::Clean,
        Phase::Merging { .. } => Phase::Merging { conflicts },
        Phase::CherryPicking { .. } => Phase::CherryPicking { conflicts },
        Phase::Reverting { .. } => Phase::Reverting { conflicts },
        Phase::Rebasing { step, total, .. } => Phase::Rebasing {
            step: *step,
            total: *total,
            conflicts,
            pause: Default::default(),
            origin: None,
        },
    }
}

fn disk(phase: Phase, lock: LockState) -> DiskState {
    DiskState {
        head: Head::Branch {
            name: BranchName("main".into()),
            oid: oid_of('a'),
        },
        phase,
        index: IndexId::Checksum(oid_of('1')),
        status: StatusSnapshot::default(),
        refs: RefsSnapshot {
            refs: [
                (RefName("refs/heads/main".into()), oid_of('a')),
                (RefName("refs/heads/other".into()), oid_of('b')),
            ]
            .into_iter()
            .collect(),
            ..RefsSnapshot::default()
        },
        lock,
        stashes: Vec::new(),
        advanced: Default::default(),
    }
}

fn repo_id() -> RepoId {
    RepoId {
        root_commit: None,
        remote_url: None,
    }
}

/// A repo whose newest read is `disk`.
fn opened(disk: DiskState) -> RepoState {
    let (state, commands) = open(repo_id());
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    step(state, Event::StateRead { read: *read, disk }).0
}

fn refused(state: &RepoState) -> Option<(Action, Refusal)> {
    match &state.notice {
        Some(Notice::Refused { action, reason }) => Some((*action, reason.clone())),
        _ => None,
    }
}

fn run_git(commands: &[Command]) -> Option<&GitArgs> {
    commands.iter().find_map(|command| match command {
        Command::RunGit { args, .. } => Some(args),
        _ => None,
    })
}

fn unresolved_count(phase: &Phase) -> usize {
    phase.conflicts().map_or(0, Conflicts::unresolved_count)
}

proptest! {
    /// The list is sorted, each path appears once, and a path that is both
    /// resolved and conflicted again is Unresolved.
    #[test]
    fn conflict_lists_are_sorted_unique_and_unresolved_wins(
        unresolved in vec((path(), kind()), 0..6),
        resolved in vec(path(), 0..6),
    ) {
        let list = Conflicts::from_parts(unresolved.clone(), resolved.clone());
        let paths: Vec<&RepoPath> = list.files().iter().map(|file| &file.path).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        sorted.dedup();
        prop_assert_eq!(&paths, &sorted);
        for (path, _) in &unresolved {
            prop_assert!(matches!(list.get(path).unwrap().state, FileState::Unresolved(_)));
        }
        for path in &resolved {
            if !unresolved.iter().any(|(other, _)| other == path) {
                prop_assert_eq!(list.get(path).unwrap().state, FileState::Resolved);
            }
        }
        prop_assert_eq!(list.unresolved_count() + list.resolved_count(), list.files().len());
        prop_assert_eq!(list.all_resolved(), list.unresolved_count() == 0);
    }

    /// Continue is enabled only when every file is Resolved: the gate, the
    /// request and the command all agree, and a refusal says how many files
    /// are left.
    #[test]
    fn continue_runs_only_when_every_file_is_resolved(phase in paused()) {
        let state = opened(disk(phase.clone(), LockState::Free));
        let remaining = unresolved_count(&phase);
        let (after, commands) = step(state, Event::ContinueRequested);
        if remaining == 0 {
            prop_assert_eq!(continue_gate(&phase), ContinueGate::Ready);
            let operation = phase.operation().unwrap();
            prop_assert_eq!(run_git(&commands), Some(&GitArgs::Continue { operation }));
            prop_assert_eq!(refused(&after), None);
        } else {
            prop_assert_eq!(continue_gate(&phase), ContinueGate::Blocked { unresolved: remaining });
            prop_assert!(commands.iter().all(|command| !command.is_write()), "{commands:?}");
            prop_assert_eq!(
                refused(&after),
                Some((
                    Action::Continue,
                    Refusal::Conflict(ConflictRefusal::Unresolved { remaining })
                ))
            );
        }
    }

    /// A Continue that was accepted while files were open is still refused
    /// when it runs: the check is against the snapshot the write runs on,
    /// not the one it was asked on.
    #[test]
    fn a_queued_continue_is_checked_when_it_runs(phase in paused()) {
        // Ready when asked...
        let ready = with_conflicts(&phase, Conflicts::default());
        // ...but a file is conflicted again by the time it is dispatched.
        let again = with_conflicts(
            &phase,
            Conflicts::from_parts([(path_named("x.txt"), ConflictKind::BothModified)], []),
        );
        let mut locked = disk(ready, LockState::HeldByOther);
        let state = opened(locked.clone());
        let (state, commands) = step(state, Event::ContinueRequested);
        prop_assert!(commands.is_empty());
        prop_assert_eq!(state.ops.queued.len(), 1);
        locked.phase = again;
        locked.lock = LockState::Free;
        let (state, commands) = step(state, Event::DiskChanged(ChangeSet([Area::InProgress].into())));
        let [Command::ReadState { read, .. }] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        let (state, commands) = step(state, Event::StateRead { read: *read, disk: locked });
        prop_assert!(commands.iter().all(|command| !command.is_write()));
        prop_assert_eq!(
            refused(&state),
            Some((
                Action::Continue,
                Refusal::Conflict(ConflictRefusal::Unresolved { remaining: 1 })
            ))
        );
        prop_assert!(state.ops.running.is_none() && state.ops.queued.is_empty());
    }

    /// Abort is always available while something is paused, whatever the
    /// files say.
    #[test]
    fn abort_is_always_allowed_in_a_paused_phase(phase in paused()) {
        let state = opened(disk(phase.clone(), LockState::Free));
        let (after, commands) = step(state, Event::AbortRequested);
        let operation = phase.operation().unwrap();
        prop_assert_eq!(run_git(&commands), Some(&GitArgs::Abort { operation }));
        prop_assert_eq!(refused(&after), None);
        // Aborting is git's own exact way back: no undo point is taken.
        prop_assert!(commands.iter().all(|command| !matches!(command, Command::SnapshotForUndo(_))));
    }

    /// Nothing is paused, nothing to continue, skip or abort.
    #[test]
    fn nothing_paused_means_nothing_to_continue_skip_or_abort(
        event in prop_oneof![
            Just(Event::ContinueRequested),
            Just(Event::SkipRequested),
            Just(Event::AbortRequested),
        ],
    ) {
        let state = opened(disk(Phase::Clean, LockState::Free));
        let (after, commands) = step(state, event);
        prop_assert!(commands.is_empty());
        let (_, reason) = refused(&after).unwrap();
        prop_assert_eq!(reason, Refusal::Conflict(ConflictRefusal::NotInProgress));
    }

    /// Skip is for a rebase only.
    #[test]
    fn skip_runs_only_for_a_rebase(phase in paused()) {
        let state = opened(disk(phase.clone(), LockState::Free));
        let (after, commands) = step(state, Event::SkipRequested);
        let operation = phase.operation().unwrap();
        if matches!(phase, Phase::Rebasing { .. }) {
            prop_assert_eq!(run_git(&commands), Some(&GitArgs::Skip { operation }));
        } else {
            prop_assert!(commands.is_empty());
            prop_assert_eq!(
                refused(&after),
                Some((Action::Skip, Refusal::Conflict(ConflictRefusal::SkipNotAvailable)))
            );
        }
    }

    /// No conflict request writes on a stale or locked snapshot (law 12):
    /// it waits in the queue and nothing runs.
    #[test]
    fn conflict_writes_wait_for_a_fresh_unlocked_snapshot(
        phase in paused(),
        event in prop_oneof![
            Just(Event::ContinueRequested),
            Just(Event::SkipRequested),
            Just(Event::AbortRequested),
            path().prop_map(|path| Event::ResolveFileRequested { path, choice: ResolveChoice::Manual }),
        ],
    ) {
        // Locked by someone else.
        let state = opened(disk(phase.clone(), LockState::HeldByOther));
        let (after, commands) = step(state, event.clone());
        prop_assert!(commands.is_empty());
        prop_assert!(after.ops.running.is_none());
        prop_assert!(refused(&after).is_none() || matches!(event, Event::SkipRequested));
        // A newer read is pending.
        let state = opened(disk(phase, LockState::Free));
        let (state, _) = step(state, Event::RefreshRequested);
        let (after, commands) = step(state, event);
        prop_assert!(commands.iter().all(|command| !command.is_write()));
        prop_assert!(after.ops.running.is_none());
    }

    /// Law 9: the run a conflict request produces names no branch, and its
    /// expectations are the snapshot's head and index.
    #[test]
    fn conflict_runs_name_no_branch_and_expect_the_snapshot(phase in paused()) {
        let snapshot = disk(phase, LockState::Free);
        for event in [Event::ContinueRequested, Event::AbortRequested, Event::SkipRequested] {
            let (_, commands) = step(opened(snapshot.clone()), event);
            for command in &commands {
                if let Command::RunGit { args, expect, .. } = command {
                    prop_assert!(args.expected_refs().is_empty());
                    prop_assert_eq!(&expect.head, &snapshot.head);
                    prop_assert_eq!(&expect.index, &snapshot.index);
                }
            }
        }
    }

    /// Network writes are paused while an operation is: a paused rebase
    /// cannot be pushed.
    #[test]
    fn push_fetch_and_pull_are_refused_while_paused(phase in paused()) {
        let main = RemoteBranch { remote: "origin".into(), branch: BranchName("main".into()) };
        let events = [
            (Event::PushRequested { to: main.clone(), force_with_lease: false, set_upstream: false }, Action::Push),
            (Event::FetchRequested { remote: "origin".into() }, Action::Fetch),
            (Event::PullRequested { from: main, mode: PullMode::Merge }, Action::Pull),
        ];
        for (event, action) in events {
            let (after, commands) = step(opened(disk(phase.clone(), LockState::Free)), event);
            prop_assert!(commands.iter().all(|command| !command.is_write()), "{action:?}");
            prop_assert_eq!(refused(&after), Some((action, Refusal::RepoBusy)));
        }
    }
}

/// What resolving a file asks git to do, for every kind of conflict.
#[test]
fn resolving_a_side_that_does_not_exist_removes_the_file() {
    use ConflictKind::*;
    let cases = [
        // (kind, ours takes the side, theirs takes the side)
        (BothModified, true, true),
        (BothAdded, true, true),
        (DeletedByUs, false, true),
        (DeletedByThem, true, false),
        (AddedByUs, true, false),
        (AddedByThem, false, true),
        (BothDeleted, false, false),
    ];
    for (kind, ours, theirs) in cases {
        let phase = Phase::Merging {
            conflicts: Conflicts::from_parts([(path_named("f.txt"), kind)], []),
        };
        for (choice, side, takes) in [
            (ResolveChoice::Ours, Side::Ours, ours),
            (ResolveChoice::Theirs, Side::Theirs, theirs),
        ] {
            let (_, commands) = step(
                opened(disk(phase.clone(), LockState::Free)),
                Event::ResolveFileRequested {
                    path: path_named("f.txt"),
                    choice,
                },
            );
            let expected = if takes {
                GitArgs::ResolveTakeSide {
                    path: path_named("f.txt"),
                    side,
                }
            } else {
                GitArgs::ResolveRemove {
                    path: path_named("f.txt"),
                }
            };
            assert_eq!(run_git(&commands), Some(&expected), "{kind:?} {choice:?}");
        }
        let (_, commands) = step(
            opened(disk(phase, LockState::Free)),
            Event::ResolveFileRequested {
                path: path_named("f.txt"),
                choice: ResolveChoice::Manual,
            },
        );
        assert_eq!(
            run_git(&commands),
            Some(&GitArgs::ResolveStage {
                path: path_named("f.txt")
            })
        );
    }
}

#[test]
fn only_a_conflicted_file_can_be_resolved() {
    let phase = Phase::Merging {
        conflicts: Conflicts::from_parts(
            [(path_named("open.txt"), ConflictKind::BothModified)],
            [path_named("done.txt")],
        ),
    };
    let state = opened(disk(phase, LockState::Free));
    let ask = |name: &str, choice| Event::ResolveFileRequested {
        path: path_named(name),
        choice,
    };
    // A path that is not in the list.
    let (after, commands) = step(state.clone(), ask("other.txt", ResolveChoice::Manual));
    assert!(commands.is_empty());
    assert_eq!(
        refused(&after),
        Some((
            Action::ResolveFile,
            Refusal::Conflict(ConflictRefusal::NotConflicted)
        ))
    );
    // An already resolved file has no stages to take a side from...
    let (after, commands) = step(state.clone(), ask("done.txt", ResolveChoice::Ours));
    assert!(commands.is_empty());
    assert_eq!(
        refused(&after),
        Some((
            Action::ResolveFile,
            Refusal::Conflict(ConflictRefusal::AlreadyResolved)
        ))
    );
    // ...but can be edited again and staged again.
    let (_, commands) = step(state, ask("done.txt", ResolveChoice::Manual));
    assert!(matches!(
        run_git(&commands),
        Some(GitArgs::ResolveStage { .. })
    ));
}

/// A double click on Skip must not skip two commits, and a Continue meant
/// for step 1 must not continue step 2.
#[test]
fn a_request_for_one_rebase_step_is_refused_on_the_next() {
    let at = |step: u32| Phase::Rebasing {
        step,
        total: 3,
        conflicts: Conflicts::from_parts([(path_named("a.txt"), ConflictKind::BothModified)], []),
        pause: Default::default(),
        origin: None,
    };
    for event in [Event::SkipRequested, Event::AbortRequested] {
        let mut locked = disk(at(1), LockState::HeldByOther);
        let state = opened(locked.clone());
        let (state, commands) = step(state, event.clone());
        assert!(commands.is_empty());
        // The lock is released and the rebase has moved on meanwhile.
        locked.lock = LockState::Free;
        locked.phase = at(2);
        let (state, commands) = step(state, Event::RefreshRequested);
        let [Command::ReadState { read, .. }] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        let (state, commands) = step(
            state,
            Event::StateRead {
                read: *read,
                disk: locked,
            },
        );
        assert!(
            commands.iter().all(|command| !command.is_write()),
            "{event:?}"
        );
        assert!(matches!(
            refused(&state),
            Some((_, Refusal::Conflict(ConflictRefusal::PhaseChanged)))
        ));
    }
}

/// Continue leaves a marker an undo stops at; resolve, skip and abort take
/// no undo point (the index cannot be saved as a tree while files are
/// conflicted).
#[test]
fn only_continue_takes_an_undo_point() {
    let ready = Phase::Merging {
        conflicts: Conflicts::from_parts([], [path_named("a.txt")]),
    };
    let (state, commands) = step(
        opened(disk(ready, LockState::Free)),
        Event::ContinueRequested,
    );
    assert!(matches!(commands[0], Command::SnapshotForUndo(_)));
    let op = state.ops.running.as_ref().unwrap().id;
    let (state, _) = step(
        state,
        Event::UndoSnapshotTaken {
            op,
            tree: oid_of('c'),
        },
    );
    let (state, commands) = step(state, Event::OpFinished(op, OpOutcome::Succeeded));
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    let (state, _) = step(
        state,
        Event::StateRead {
            read: *read,
            disk: disk(Phase::Clean, LockState::Free),
        },
    );
    assert_eq!(
        state.undo.entries.back().map(|entry| entry.reversible),
        Some(Reversible::No(Irreversible::Continued))
    );
    let (state, _) = step(state, Event::UndoRequested);
    assert_eq!(
        refused(&state),
        Some((Action::Undo, Refusal::NotUndoable(Irreversible::Continued)))
    );

    let open_file = Phase::Merging {
        conflicts: Conflicts::from_parts([(path_named("a.txt"), ConflictKind::BothModified)], []),
    };
    for event in [
        Event::ResolveFileRequested {
            path: path_named("a.txt"),
            choice: ResolveChoice::Manual,
        },
        Event::AbortRequested,
    ] {
        let (state, commands) = step(opened(disk(open_file.clone(), LockState::Free)), event);
        assert!(
            commands
                .iter()
                .all(|command| !matches!(command, Command::SnapshotForUndo(_)))
        );
        assert!(run_git(&commands).is_some());
        assert_eq!(state.undo.pending, UndoPending::Idle);
    }
}

/// A running conflict operation is not cancelled: killing an abort or a
/// continue half way would leave git between two states of its own.
#[test]
fn a_running_conflict_operation_cannot_be_cancelled() {
    let phase = Phase::Merging {
        conflicts: Conflicts::from_parts([(path_named("a.txt"), ConflictKind::BothModified)], []),
    };
    let (state, _) = step(opened(disk(phase, LockState::Free)), Event::AbortRequested);
    let op = state.ops.running.as_ref().unwrap().id;
    let (state, commands) = step(state, Event::OpCancelRequested(op));
    assert!(commands.is_empty());
    assert!(matches!(
        state.ops.running.as_ref().unwrap().phase,
        OpPhase::Running { .. }
    ));
}

/// Where conflict content says a side is missing, binary is flagged.
#[test]
fn binary_on_any_side_is_flagged() {
    let content = |theirs| ConflictContent {
        path: path_named("img.png"),
        base: StageContent::Absent,
        ours: StageContent::Text("a".into()),
        theirs,
    };
    assert!(!content(StageContent::Text("b".into())).is_binary());
    assert!(content(StageContent::Binary { size: 4 }).is_binary());
}

// --- P2: starting a cherry-pick or a revert ---------------------------------

fn replay_event(pick: bool, commit: Oid) -> Event {
    if pick {
        Event::CherryPickRequested(commit)
    } else {
        Event::RevertRequested(commit)
    }
}

fn replay_action(pick: bool) -> Action {
    if pick {
        Action::CherryPick
    } else {
        Action::Revert
    }
}

proptest! {
    /// A clean repo: one git run for the commit asked, expecting the
    /// snapshot's head and index, with an undo point that restores `HEAD`'s
    /// ref only (law 9) and the index (law 8).
    #[test]
    fn a_pick_or_revert_runs_once_and_records_only_head(pick in any::<bool>(), c in proptest::sample::select(vec!['b', 'c', 'd', 'f'])) {
        let snapshot = disk(Phase::Clean, LockState::Free);
        let commit = oid_of(c);
        let (state, commands) = step(opened(snapshot.clone()), replay_event(pick, commit.clone()));
        let expected_args = if pick {
            GitArgs::CherryPick { commit: commit.clone() }
        } else {
            GitArgs::Revert { commit: commit.clone() }
        };
        prop_assert_eq!(run_git(&commands), Some(&expected_args));
        let Some(Command::SnapshotForUndo(point)) = commands.first() else {
            panic!("{commands:?}");
        };
        prop_assert_eq!(&point.refs, &vec![RefValue {
            name: RefName("refs/heads/main".into()),
            oid: Some(oid_of('a')),
        }]);
        prop_assert_eq!(&point.reverse, &Reversal::Refs);
        prop_assert_eq!(point.index_effect, IndexEffect::Rewrites);
        for command in &commands {
            if let Command::RunGit { args, expect, .. } = command {
                prop_assert!(args.expected_refs().is_empty());
                prop_assert_eq!(&expect.head, &snapshot.head);
                prop_assert_eq!(&expect.index, &snapshot.index);
            }
        }
        prop_assert!(state.ops.running.is_some());
    }

    /// Nothing starts while a merge, rebase, cherry-pick or revert is paused.
    #[test]
    fn a_pick_or_revert_is_refused_while_something_is_paused(
        phase in paused(),
        pick in any::<bool>(),
    ) {
        let (state, commands) = step(
            opened(disk(phase, LockState::Free)),
            replay_event(pick, oid_of('b')),
        );
        prop_assert!(commands.iter().all(|command| !command.is_write()));
        prop_assert_eq!(refused(&state), Some((replay_action(pick), Refusal::RepoBusy)));
        prop_assert!(state.ops.running.is_none() && state.ops.queued.is_empty());
    }

    /// Law 12: with `index.lock` held by someone else the request waits and
    /// nothing is written.
    #[test]
    fn a_pick_or_revert_waits_for_the_lock(pick in any::<bool>()) {
        let state = opened(disk(Phase::Clean, LockState::HeldByOther));
        let (state, commands) = step(state, replay_event(pick, oid_of('b')));
        prop_assert!(commands.is_empty());
        prop_assert_eq!(state.ops.queued.len(), 1);
        prop_assert!(state.ops.running.is_none());
    }

    /// Law 9: if `HEAD` moved to another branch (or went detached) between
    /// the request and its dispatch, the request is refused.
    #[test]
    fn a_pick_or_revert_is_refused_when_head_moved(pick in any::<bool>(), detach in any::<bool>()) {
        let mut moved = disk(Phase::Clean, LockState::Free);
        let state = opened(disk(Phase::Clean, LockState::HeldByOther));
        let (state, _) = step(state, replay_event(pick, oid_of('b')));
        moved.head = if detach {
            Head::Detached(oid_of('a'))
        } else {
            Head::Branch { name: BranchName("other".into()), oid: oid_of('b') }
        };
        let (state, commands) = step(state, Event::DiskChanged(ChangeSet([Area::Head].into())));
        let [Command::ReadState { read, .. }] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        let (state, commands) = step(state, Event::StateRead { read: *read, disk: moved });
        prop_assert!(commands.iter().all(|command| !command.is_write()));
        prop_assert_eq!(refused(&state), Some((replay_action(pick), Refusal::BranchChanged)));
    }
}

/// A detached `HEAD` is a fine place to pick onto; its undo point records
/// `HEAD` itself.
#[test]
fn a_pick_onto_a_detached_head_records_head() {
    let mut snapshot = disk(Phase::Clean, LockState::Free);
    snapshot.head = Head::Detached(oid_of('a'));
    let (_, commands) = step(opened(snapshot), Event::CherryPickRequested(oid_of('b')));
    let Some(Command::SnapshotForUndo(point)) = commands.first() else {
        panic!("{commands:?}");
    };
    assert_eq!(point.refs[0].name, RefName("HEAD".into()));
    assert_eq!(point.head, HeadTarget::Detached);
}

#[test]
fn a_pick_or_revert_needs_a_commit_and_a_read_snapshot() {
    let mut unborn = disk(Phase::Clean, LockState::Free);
    unborn.head = Head::Unborn(BranchName("main".into()));
    for pick in [true, false] {
        let (state, commands) = step(opened(unborn.clone()), replay_event(pick, oid_of('b')));
        assert!(commands.is_empty());
        assert_eq!(
            refused(&state),
            Some((
                replay_action(pick),
                Refusal::Branch(BranchRefusal::NoCommitsYet)
            ))
        );
        let (state, _) = step(open(repo_id()).0, replay_event(pick, oid_of('b')));
        assert_eq!(
            refused(&state),
            Some((replay_action(pick), Refusal::NotRead))
        );
    }
}

/// A pick that stops on a conflict ends as a failed run: its undo point is
/// dropped (abort is the way back) and the snapshot read next shows the
/// paused operation. A pick that succeeds leaves an undoable entry.
#[test]
fn a_conflicted_pick_leaves_no_undo_point_and_a_clean_one_leaves_one() {
    let run = |pick: bool, outcome: OpOutcome, after: DiskState| {
        let (state, commands) = step(
            opened(disk(Phase::Clean, LockState::Free)),
            replay_event(pick, oid_of('b')),
        );
        let op = state.ops.running.as_ref().unwrap().id;
        let Some(Command::SnapshotForUndo(_)) = commands.first() else {
            panic!("{commands:?}");
        };
        let (state, _) = step(
            state,
            Event::UndoSnapshotTaken {
                op,
                tree: oid_of('9'),
            },
        );
        let (state, commands) = step(state, Event::OpFinished(op, outcome));
        let [.., Command::ReadState { read, .. }] = commands.as_slice() else {
            panic!("{commands:?}");
        };
        let read = *read;
        let (state, _) = step(state, Event::StateRead { read, disk: after });
        state
    };
    for pick in [true, false] {
        let conflicted = Phase::CherryPicking {
            conflicts: Conflicts::from_parts(
                [(path_named("a.txt"), ConflictKind::BothModified)],
                [],
            ),
        };
        let conflicted = if pick {
            conflicted
        } else {
            Phase::Reverting {
                conflicts: conflicted.conflicts().unwrap().clone(),
            }
        };
        let failed = OpOutcome::Failed(Failure {
            class: ErrorClass::Expected,
            details: "CONFLICT".into(),
        });
        let state = run(pick, failed, disk(conflicted.clone(), LockState::Free));
        assert!(state.undo.entries.is_empty());
        assert_eq!(state.undo.pending, UndoPending::Idle);
        assert_eq!(state.snapshot.disk().unwrap().phase, conflicted);

        let mut moved = disk(Phase::Clean, LockState::Free);
        moved.head = Head::Branch {
            name: BranchName("main".into()),
            oid: oid_of('c'),
        };
        let state = run(pick, OpOutcome::Succeeded, moved);
        assert_eq!(state.undo.entries.len(), 1);
        assert_eq!(state.undo.entries[0].reversible, Reversible::Yes);
    }
}

/// Killing git half way through a pick would leave its sequencer between two
/// states: not cancellable, like the rest of the conflict flow.
#[test]
fn a_running_pick_or_revert_cannot_be_cancelled() {
    for pick in [true, false] {
        let (state, _) = step(
            opened(disk(Phase::Clean, LockState::Free)),
            replay_event(pick, oid_of('b')),
        );
        let op = state.ops.running.as_ref().unwrap().id;
        let (state, commands) = step(state, Event::OpCancelRequested(op));
        assert!(commands.is_empty());
        assert!(matches!(
            state.ops.running.as_ref().unwrap().phase,
            OpPhase::Running { .. }
        ));
    }
}

/// A change to a tracked file, staged or not, refuses the start; an
/// untracked file does not.
#[test]
fn a_pick_or_revert_needs_tracked_files_unchanged() {
    let entry = |staged, unstaged| StatusEntry {
        path: path_named("a.txt"),
        staged,
        unstaged,
    };
    for pick in [true, false] {
        for (entries, refused_with) in [
            (vec![entry(Some(FileChange::Modified), None)], true),
            (vec![entry(None, Some(FileChange::Modified))], true),
            (vec![entry(None, Some(FileChange::Deleted))], true),
            (vec![entry(None, Some(FileChange::Untracked))], false),
        ] {
            let mut snapshot = disk(Phase::Clean, LockState::Free);
            snapshot.status = StatusSnapshot::from_entries(entries);
            let (state, commands) = step(opened(snapshot), replay_event(pick, oid_of('b')));
            if refused_with {
                assert!(commands.iter().all(|command| !command.is_write()));
                assert_eq!(
                    refused(&state),
                    Some((replay_action(pick), Refusal::UncommittedChanges))
                );
            } else {
                assert!(run_git(&commands).is_some());
            }
        }
    }
}
