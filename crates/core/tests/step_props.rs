//! Property tests for `step` over random event sequences. Op and read ids are
//! drawn from small ranges, so events often hit live operations and reads.

use invoke_core::*;
use proptest::collection::{btree_map, btree_set, vec};
use proptest::prelude::*;

fn oid() -> impl Strategy<Value = Oid> {
    "[0-9a-f]{40}".prop_map(|hex| Oid::from_hex(&hex).unwrap())
}

fn branch() -> impl Strategy<Value = BranchName> {
    prop_oneof![Just("main"), Just("dev"), Just("feature/x")]
        .prop_map(|name| BranchName(name.into()))
}

fn path() -> impl Strategy<Value = RepoPath> {
    "[a-z]{1,3}(/[a-z]{1,3})?".prop_map(RepoPath)
}

fn head() -> impl Strategy<Value = Head> {
    prop_oneof![
        (branch(), oid()).prop_map(|(name, oid)| Head::Branch { name, oid }),
        oid().prop_map(Head::Detached),
        branch().prop_map(Head::Unborn),
    ]
}

fn conflicts() -> impl Strategy<Value = Conflicts> {
    vec(path(), 0..3).prop_map(|paths| Conflicts { paths })
}

fn phase() -> impl Strategy<Value = Phase> {
    prop_oneof![
        Just(Phase::Clean),
        conflicts().prop_map(|conflicts| Phase::Merging { conflicts }),
        (0..5u32, 0..5u32, conflicts()).prop_map(|(step, total, conflicts)| Phase::Rebasing {
            step,
            total,
            conflicts
        }),
        conflicts().prop_map(|conflicts| Phase::CherryPicking { conflicts }),
        conflicts().prop_map(|conflicts| Phase::Reverting { conflicts }),
    ]
}

fn file_change() -> impl Strategy<Value = FileChange> {
    prop_oneof![
        Just(FileChange::Added),
        Just(FileChange::Modified),
        Just(FileChange::Deleted),
        path().prop_map(|from| FileChange::Renamed { from }),
        Just(FileChange::TypeChanged),
        Just(FileChange::Untracked),
    ]
}

fn disk() -> impl Strategy<Value = DiskState> {
    let status = vec(
        (
            path(),
            proptest::option::of(file_change()),
            proptest::option::of(file_change()),
        ),
        0..4,
    )
    .prop_map(|entries| StatusSnapshot {
        entries: entries
            .into_iter()
            .map(|(path, staged, unstaged)| StatusEntry {
                path,
                staged,
                unstaged,
            })
            .collect(),
    });
    let refs = btree_map("refs/heads/[a-c]".prop_map(RefName), oid(), 0..3)
        .prop_map(|refs| RefsSnapshot { refs });
    let index = prop_oneof![3 => Just(IndexId::Absent), 1 => oid().prop_map(IndexId::Checksum)];
    // Weighted towards Free so writes get dispatched often.
    let lock = prop_oneof![
        3 => Just(LockState::Free),
        1 => Just(LockState::HeldByUs),
        1 => Just(LockState::HeldByOther),
    ];
    (head(), phase(), index, status, refs, lock).prop_map(
        |(head, phase, index, status, refs, lock)| DiskState {
            head,
            phase,
            index,
            status,
            refs,
            lock,
        },
    )
}

fn failure() -> impl Strategy<Value = Failure> {
    prop_oneof![
        Just(ErrorClass::Expected),
        Just(ErrorClass::PreconditionChanged),
        Just(ErrorClass::Auth),
        Just(ErrorClass::Environment),
        Just(ErrorClass::Network),
        Just(ErrorClass::Bug),
    ]
    .prop_map(|class| Failure {
        class,
        details: "stderr".into(),
    })
}

fn outcome() -> impl Strategy<Value = OpOutcome> {
    prop_oneof![
        Just(OpOutcome::Succeeded),
        failure().prop_map(OpOutcome::Failed),
        Just(OpOutcome::Cancelled),
    ]
}

fn selection() -> impl Strategy<Value = Selection> {
    prop_oneof![
        path().prop_map(Selection::File),
        (path(), 0..3usize).prop_map(|(path, hunk)| Selection::Hunk { path, hunk }),
        (path(), 0..3usize, vec(0..9usize, 0..3))
            .prop_map(|(path, hunk, lines)| Selection::Lines { path, hunk, lines }),
    ]
}

/// Task 11: a stage request. The patch is opaque text to the core (an
/// empty one is refused); `based_on` is usually `Absent`, which is also
/// the usual index of `disk()`, so about half the requests match.
fn stage_request() -> impl Strategy<Value = StageRequest> {
    (
        selection(),
        prop_oneof![Just(String::new()), "[ -~]{1,12}"],
        prop_oneof![3 => Just(IndexId::Absent), 1 => oid().prop_map(IndexId::Checksum)],
    )
        .prop_map(|(selection, patch, based_on)| StageRequest {
            selection,
            patch: Patch(patch),
            based_on,
        })
}

fn op_id() -> impl Strategy<Value = OpId> {
    (0..6u64).prop_map(OpId)
}

fn read_id() -> impl Strategy<Value = ReadId> {
    (0..12u64).prop_map(ReadId)
}

fn prompt() -> impl Strategy<Value = AuthPrompt> {
    prop_oneof![
        Just(AuthKind::Username),
        Just(AuthKind::Password),
        Just(AuthKind::Passphrase),
        Just(AuthKind::HostKey),
    ]
    .prop_map(|kind| AuthPrompt {
        kind,
        text: "Password for 'https://example.com':".into(),
    })
}

fn event() -> impl Strategy<Value = Event> {
    let areas = btree_set(
        prop_oneof![
            Just(Area::Worktree),
            Just(Area::Index),
            Just(Area::Head),
            Just(Area::Refs),
            Just(Area::InProgress),
        ],
        0..3,
    );
    prop_oneof![
        1 => stage_request().prop_map(Event::StageRequested),
        1 => stage_request().prop_map(Event::UnstageRequested),
        3 => ("[a-z ]{0,8}", any::<bool>())
            .prop_map(|(message, amend)| Event::CommitRequested { message, amend }),
        1 => any::<bool>().prop_map(|force_with_lease| Event::PushRequested { force_with_lease }),
        1 => op_id().prop_map(Event::OpCancelRequested),
        1 => op_id().prop_map(|op| Event::AuthAnswered(op, AuthAnswer("secret".into()))),
        1 => Just(Event::UndoRequested),
        1 => Just(Event::RefreshRequested),
        1 => Just(Event::NoticeDismissed),
        2 => areas.prop_map(|areas| Event::DiskChanged(ChangeSet(areas))),
        4 => (read_id(), disk()).prop_map(|(read, disk)| Event::StateRead { read, disk }),
        1 => (read_id(), failure()).prop_map(|(read, failure)| Event::ReadFailed { read, failure }),
        1 => (op_id(), 0..100u64).prop_map(|(op, done)| Event::OpProgress(
            op,
            Progress {
                stage: "Receiving objects".into(),
                done,
                total: Some(100),
            }
        )),
        1 => (op_id(), prompt()).prop_map(|(op, prompt)| Event::AuthNeeded(op, prompt)),
        3 => (op_id(), outcome()).prop_map(|(op, outcome)| Event::OpFinished(op, outcome)),
        2 => (op_id(), oid()).prop_map(|(op, tree)| Event::UndoSnapshotTaken { op, tree }),
    ]
}

fn repo_id() -> RepoId {
    RepoId {
        root_commit: None,
        remote_url: None,
    }
}

/// Runs `open` and then every event, calling `check(before, event, after,
/// commands)` on each step.
fn run(
    events: Vec<Event>,
    mut check: impl FnMut(&RepoState, &Event, &RepoState, &[Command]) -> Result<(), TestCaseError>,
) -> Result<(), TestCaseError> {
    let (mut state, _) = open(repo_id());
    for event in events {
        let before = state.clone();
        let (after, commands) = step(state, event.clone());
        check(&before, &event, &after, &commands)?;
        state = after;
    }
    Ok(())
}

fn applied_read(state: &RepoState) -> Option<ReadId> {
    match state.snapshot {
        Snapshot::NotRead => None,
        Snapshot::Read { read, .. } => Some(read),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// `step` is total: no event sequence makes it panic.
    #[test]
    fn step_never_panics(events in vec(event(), 0..60)) {
        run(events, |_, _, _, _| Ok(()))?;
    }

    /// Law 11 (core half): a disk change never patches the snapshot; it only
    /// asks for a read, and the snapshot is no longer fresh until it lands.
    #[test]
    fn disk_changes_only_request_a_read(events in vec(event(), 0..60)) {
        run(events, |before, event, after, commands| {
            if let Event::DiskChanged(ChangeSet(areas)) = event {
                prop_assert_eq!(&after.snapshot, &before.snapshot);
                let reads: Vec<_> = commands
                    .iter()
                    .filter(|command| matches!(command, Command::ReadState { .. }))
                    .collect();
                if areas.is_empty() {
                    prop_assert!(reads.is_empty());
                } else {
                    prop_assert_eq!(reads.len(), 1);
                    prop_assert!(after.fresh_disk().is_none());
                }
            }
            Ok(())
        })?;
    }

    /// Law 11 (core half): only a newer read replaces the snapshot, and then
    /// with exactly what was read. Older reads arriving late are dropped.
    #[test]
    fn newest_read_wins(events in vec(event(), 0..60)) {
        run(events, |before, event, after, _| {
            let (old, new) = (applied_read(before), applied_read(after));
            prop_assert!(new >= old, "snapshot regressed from {old:?} to {new:?}");
            if let Event::StateRead { read, disk } = event {
                let requested = *read < before.reads.next;
                let newer = old.is_none_or(|old| *read > old);
                if requested && newer {
                    prop_assert_eq!(after.snapshot.disk(), Some(disk));
                    prop_assert_eq!(new, Some(*read));
                } else {
                    prop_assert_eq!(&after.snapshot, &before.snapshot);
                }
            } else {
                prop_assert_eq!(&after.snapshot, &before.snapshot);
            }
            Ok(())
        })?;
    }

    /// Law 12 (core half) and rule 5: a write is emitted only on a fresh
    /// snapshot whose lock is free, carries that snapshot's head and index,
    /// and never while another write is in flight.
    #[test]
    fn writes_need_a_fresh_unlocked_snapshot(events in vec(event(), 0..60)) {
        run(events, |before, _, after, commands| {
            let writes: Vec<_> = commands.iter().filter(|command| command.is_write()).collect();
            if writes.is_empty() {
                return Ok(());
            }
            prop_assert!(before.ops.running.is_none(), "write started while another runs");
            let disk = after.fresh_disk();
            prop_assert!(disk.is_some(), "write on a stale snapshot");
            let disk = disk.unwrap();
            prop_assert_eq!(disk.lock, LockState::Free);
            let running = after.ops.running.as_ref();
            prop_assert!(running.is_some());
            let running = running.unwrap();
            for write in writes {
                match write {
                    Command::RunGit { op, expect, .. } => {
                        prop_assert_eq!(*op, running.id);
                        prop_assert_eq!(&expect.head, &disk.head);
                        prop_assert_eq!(&expect.index, &disk.index);
                    }
                    Command::SnapshotForUndo(point) => {
                        prop_assert_eq!(point.op, running.id);
                        prop_assert_eq!(&point.index, &disk.index);
                    }
                    Command::ReplayUndo(replay) => {
                        prop_assert_eq!(replay.op, running.id);
                        prop_assert_eq!(&replay.expect.head, &disk.head);
                        prop_assert_eq!(&replay.expect.index, &disk.index);
                        for item in &replay.refs {
                            prop_assert_eq!(
                                &item.expect,
                                &invoke_core::undo::current_value(disk, &item.name)
                            );
                        }
                    }
                    Command::ApplyPatchToIndex { op, expect_index, .. } => {
                        prop_assert_eq!(*op, running.id);
                        prop_assert_eq!(expect_index, &disk.index);
                    }
                    _ => unreachable!(),
                }
            }
            Ok(())
        })?;
    }

    /// The stack never holds more than 50 points, and an undo that was
    /// refused or is waiting leaves it alone.
    #[test]
    fn undo_stack_stays_bounded(events in vec(event(), 0..60)) {
        run(events, |_, _, after, _| {
            prop_assert!(after.undo.entries.len() <= 50);
            Ok(())
        })?;
    }

    /// Every request is queued or refused with a stated reason; none is
    /// dropped silently.
    #[test]
    fn requests_are_queued_or_refused(events in vec(event(), 0..60)) {
        run(events, |before, event, after, _| {
            let action = match event {
                Event::StageRequested(_) => Action::Stage,
                Event::UnstageRequested(_) => Action::Unstage,
                Event::CommitRequested { .. } => Action::Commit,
                Event::PushRequested { .. } => Action::Push,
                Event::UndoRequested => Action::Undo,
                _ => return Ok(()),
            };
            let refused = matches!(
                after.notice,
                Some(Notice::Refused { action: refused, .. }) if refused == action
            );
            let accepted = after.ops.next.0 == before.ops.next.0 + 1
                && (after.ops.queued.back().map(|queued| queued.id)
                    == Some(before.ops.next)
                    || after.ops.running.as_ref().map(|running| running.id)
                        == Some(before.ops.next));
            // An accepted request may leave an older refusal of the same
            // action in `notice`, so only "neither" is a failure.
            prop_assert!(refused || accepted, "{event:?}: refused={refused} accepted={accepted}");
            Ok(())
        })?;
    }

    /// Law 13 (core half): when a running operation ends, however it ends,
    /// a full read follows and no write starts before it lands.
    #[test]
    fn finished_ops_are_followed_by_a_full_read(events in vec(event(), 0..60)) {
        run(events, |before, event, after, commands| {
            if let Event::OpFinished(op, _) = event
                && before.ops.running.as_ref().is_some_and(|running| running.id == *op)
            {
                prop_assert!(after.ops.running.is_none());
                let full_read = commands.iter().any(|command| matches!(
                    command,
                    Command::ReadState { scope, .. } if *scope == ReadScope::full()
                ));
                prop_assert!(full_read, "no full read after {:?}", event);
                prop_assert!(!commands.iter().any(Command::is_write));
            }
            Ok(())
        })?;
    }
}

/// The happy path end to end: open, read, commit, finish, re-read.
#[test]
fn commit_round_trip() {
    let (state, commands) = open(repo_id());
    let Command::ReadState { read, .. } = commands[0].clone() else {
        panic!("open must read first: {commands:?}");
    };
    let oid = Oid::from_hex(&"a".repeat(40)).unwrap();
    let index = IndexId::Checksum(Oid::from_hex(&"b".repeat(40)).unwrap());
    let disk = DiskState {
        head: Head::Branch {
            name: BranchName("main".into()),
            oid: oid.clone(),
        },
        phase: Phase::Clean,
        index: index.clone(),
        status: StatusSnapshot::default(),
        refs: RefsSnapshot::default(),
        lock: LockState::HeldByOther,
    };

    // Locked by someone else: the commit waits.
    let (state, _) = step(
        state,
        Event::StateRead {
            read,
            disk: disk.clone(),
        },
    );
    let (state, commands) = step(
        state,
        Event::CommitRequested {
            message: "Fix login\n\nBody".into(),
            amend: false,
        },
    );
    assert!(commands.is_empty(), "{commands:?}");
    let op = state.ops.queued[0].id;

    // The watcher sees the lock go away; the fresh read releases the commit.
    let (state, commands) = step(state, Event::DiskChanged(ChangeSet([Area::Index].into())));
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    let free = DiskState {
        lock: LockState::Free,
        ..disk
    };
    let (state, commands) = step(
        state,
        Event::StateRead {
            read: *read,
            disk: free.clone(),
        },
    );
    assert_eq!(
        commands,
        vec![
            Command::SnapshotForUndo(UndoPoint {
                op,
                label: "Commit 'Fix login'".into(),
                head: HeadTarget::Branch(BranchName("main".into())),
                refs: vec![RefValue {
                    name: RefName("refs/heads/main".into()),
                    oid: Some(oid.clone()),
                }],
                index: index.clone(),
            }),
            Command::RunGit {
                op,
                args: GitArgs::Commit {
                    message: "Fix login\n\nBody".into(),
                    amend: false,
                },
                expect: Expect {
                    head: Head::Branch {
                        name: BranchName("main".into()),
                        oid,
                    },
                    index,
                },
            },
        ]
    );

    let tree = Oid::from_hex(&"c".repeat(40)).unwrap();
    let (state, _) = step(
        state,
        Event::UndoSnapshotTaken {
            op,
            tree: tree.clone(),
        },
    );
    let (state, commands) = step(state, Event::OpFinished(op, OpOutcome::Succeeded));
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    assert!(state.ops.running.is_none());
    assert_eq!(
        state.notice,
        Some(Notice::OpEnded {
            op,
            outcome: OpOutcome::Succeeded
        })
    );

    // The point is recorded once the fresh read shows what the commit left.
    assert!(state.undo.entries.is_empty());
    let new_oid = Oid::from_hex(&"d".repeat(40)).unwrap();
    let new_index = IndexId::Checksum(Oid::from_hex(&"e".repeat(40)).unwrap());
    let after = DiskState {
        head: Head::Branch {
            name: BranchName("main".into()),
            oid: new_oid.clone(),
        },
        index: new_index.clone(),
        refs: RefsSnapshot {
            refs: [(RefName("refs/heads/main".into()), new_oid.clone())].into(),
        },
        lock: LockState::Free,
        ..free
    };
    let (state, commands) = step(
        state,
        Event::StateRead {
            read: *read,
            disk: after,
        },
    );
    assert!(commands.is_empty(), "{commands:?}");
    let entry = state.undo.entries.back().cloned().expect("an undo point");
    assert_eq!(entry.tree, tree);
    assert_eq!(entry.point.label, "Commit 'Fix login'");
    assert_eq!(entry.after_index, new_index);
    assert_eq!(entry.after_refs[0].oid, Some(new_oid));

    // Undo plans the replay from the point, with expected values.
    let (state, commands) = step(state, Event::UndoRequested);
    let [Command::ReplayUndo(replay)] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    assert_eq!(replay.tree, tree);
    assert_eq!(
        replay.refs[0].restore.as_ref(),
        Some(&entry.point.refs[0].oid.clone().unwrap())
    );
    assert!(state.undo.entries.is_empty());
}

#[test]
fn auth_answer_is_redacted_in_debug_output() {
    let event = Event::AuthAnswered(OpId(0), AuthAnswer("hunter2".into()));
    assert!(!format!("{event:?}").contains("hunter2"));
}
