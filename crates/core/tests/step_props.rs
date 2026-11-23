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

fn remote_branch() -> impl Strategy<Value = RemoteBranch> {
    branch().prop_map(|branch| RemoteBranch {
        remote: "origin".into(),
        branch,
    })
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
    let kind = prop_oneof![
        Just(ConflictKind::BothModified),
        Just(ConflictKind::BothAdded),
        Just(ConflictKind::DeletedByUs),
        Just(ConflictKind::DeletedByThem),
    ];
    (vec((path(), kind), 0..3), vec(path(), 0..3))
        .prop_map(|(unresolved, resolved)| Conflicts::from_parts(unresolved, resolved))
}

fn phase() -> impl Strategy<Value = Phase> {
    prop_oneof![
        Just(Phase::Clean),
        conflicts().prop_map(|conflicts| Phase::Merging { conflicts }),
        (0..5u32, 0..5u32, conflicts()).prop_map(|(step, total, conflicts)| Phase::Rebasing {
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
    let refs = btree_map(
        "refs/(heads/[a-c]|remotes/origin/(main|dev))".prop_map(RefName),
        oid(),
        0..4,
    )
    .prop_map(|refs| RefsSnapshot {
        refs,
        ..RefsSnapshot::default()
    });
    let index = prop_oneof![3 => Just(IndexId::Absent), 1 => oid().prop_map(IndexId::Checksum)];
    // Weighted towards Free so writes get dispatched often.
    let lock = prop_oneof![
        3 => Just(LockState::Free),
        1 => Just(LockState::HeldByUs),
        1 => Just(LockState::HeldByOther),
    ];
    // Stashes come from a small pool of ids, so stash requests (drawn from
    // the same pool) often name an entry that exists.
    let stashes = vec(stash_oid(), 0..3).prop_map(|ids| {
        ids.into_iter()
            .map(|id| StashEntry {
                id,
                message: "On main: wip".into(),
            })
            .collect::<Vec<_>>()
    });
    (head(), phase(), index, status, refs, stashes, lock).prop_map(
        |(head, phase, index, status, refs, stashes, lock)| DiskState {
            head,
            phase,
            index,
            status,
            refs,
            stashes,
            lock,
        },
    )
}

fn stash_oid() -> impl Strategy<Value = Oid> {
    (0..4u8).prop_map(|k| Oid::from_hex(&format!("{:040x}", 0xa000 + u64::from(k))).unwrap())
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
        2 => (remote_branch(), any::<bool>(), any::<bool>()).prop_map(
            |(to, force_with_lease, set_upstream)| Event::PushRequested {
                to,
                force_with_lease,
                set_upstream
            }
        ),
        1 => Just(Event::FetchRequested { remote: "origin".into() }),
        1 => (remote_branch(), any::<bool>()).prop_map(|(from, rebase)| Event::PullRequested {
            from,
            mode: if rebase { PullMode::Rebase } else { PullMode::Merge },
        }),
        1 => remote_branch().prop_map(|upstream| Event::SetUpstreamRequested { upstream }),
        1 => op_id().prop_map(Event::OpCancelRequested),
        1 => op_id().prop_map(|op| Event::AuthAnswered(op, AuthAnswer("secret".into()))),
        1 => Just(Event::UndoRequested),
        // Task 15: branches (some names are not valid on purpose).
        2 => ("[a-c/ ]{0,3}", any::<bool>(), any::<bool>(), oid()).prop_map(
            |(name, switch, here, commit)| Event::BranchCreateRequested {
                name,
                at: if here { BranchStart::Head } else { BranchStart::Commit(commit) },
                switch,
            }
        ),
        1 => branch().prop_map(Event::BranchSwitchRequested),
        1 => (branch(), "[a-c/ ]{0,3}")
            .prop_map(|(from, to)| Event::BranchRenameRequested { from, to }),
        1 => (branch(), any::<bool>())
            .prop_map(|(name, force)| Event::BranchDeleteRequested { name, force }),
        1 => ("[a-z \\n]{0,8}", any::<bool>()).prop_map(|(message, include_untracked)| {
            Event::StashSaveRequested { message, include_untracked }
        }),
        1 => stash_oid().prop_map(Event::StashApplyRequested),
        1 => stash_oid().prop_map(Event::StashPopRequested),
        1 => stash_oid().prop_map(Event::StashDropRequested),
        // Task 23: the conflict flow.
        2 => (path(), prop_oneof![
            Just(ResolveChoice::Ours),
            Just(ResolveChoice::Theirs),
            Just(ResolveChoice::Manual),
        ]).prop_map(|(path, choice)| Event::ResolveFileRequested { path, choice }),
        2 => Just(Event::ContinueRequested),
        1 => Just(Event::SkipRequested),
        2 => Just(Event::AbortRequested),
        1 => Just(Event::RefreshRequested),
        1 => Just(Event::NoticeDismissed),
        1 => Just(Event::RemoveStaleLockRequested),
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
                    Command::RunGit { op, expect, args } => {
                        prop_assert_eq!(*op, running.id);
                        prop_assert_eq!(&expect.head, &disk.head);
                        prop_assert_eq!(&expect.index, &disk.index);
                        // Rule 5 for refs: every ref the run names is
                        // expected at the value the snapshot holds.
                        for item in args.expected_refs() {
                            prop_assert_eq!(disk.refs.refs.get(&item.name), item.oid.as_ref());
                        }
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
                Event::FetchRequested { .. } => Action::Fetch,
                Event::PullRequested { .. } => Action::Pull,
                Event::SetUpstreamRequested { .. } => Action::SetUpstream,
                Event::UndoRequested => Action::Undo,
                Event::BranchCreateRequested { .. } => Action::CreateBranch,
                Event::BranchSwitchRequested(_) => Action::SwitchBranch,
                Event::BranchRenameRequested { .. } => Action::RenameBranch,
                Event::BranchDeleteRequested { .. } => Action::DeleteBranch,
                Event::StashSaveRequested { .. } => Action::StashSave,
                Event::StashApplyRequested(_) => Action::StashApply,
                Event::StashPopRequested(_) => Action::StashPop,
                Event::StashDropRequested(_) => Action::StashDrop,
                Event::ResolveFileRequested { .. } => Action::ResolveFile,
                Event::ContinueRequested => Action::Continue,
                Event::SkipRequested => Action::Skip,
                Event::AbortRequested => Action::Abort,
                _ => return Ok(()),
            };
            let accepted = after.ops.next.0 == before.ops.next.0 + 1
                && (after.ops.queued.back().map(|queued| queued.id)
                    == Some(before.ops.next)
                    || after.ops.running.as_ref().map(|running| running.id)
                        == Some(before.ops.next));
            // A refusal notice left over from an earlier request does not
            // count against an accepted one.
            let refused = matches!(
                after.notice,
                Some(Notice::Refused { action: refused, .. }) if refused == action
            ) && !(accepted && before.notice == after.notice);
            prop_assert!(refused != accepted, "{event:?}: refused={refused} accepted={accepted}");
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
        // Task 14: a plain commit needs something staged.
        status: StatusSnapshot::from_entries([StatusEntry {
            path: RepoPath("a.txt".into()),
            staged: Some(FileChange::Modified),
            unstaged: None,
        }]),
        refs: RefsSnapshot::default(),
        stashes: Vec::new(),
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
                index_effect: IndexEffect::Rewrites,
                reverse: Reversal::Refs,
            }),
            Command::RunGit {
                op,
                args: GitArgs::Commit {
                    // Cleaned: one newline at the end (task 14).
                    message: "Fix login\n\nBody\n".into(),
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
            ..RefsSnapshot::default()
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

// Task 17: fetch, pull, push and set-upstream (laws 9, 10, 13).

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Law 10 (core half): a force push is only ever started with a lease
    /// whose expected value is the remote-tracking value in the snapshot
    /// the request was made against; a force push without a known remote
    /// value is refused, and a plain push never carries a lease.
    #[test]
    fn force_push_is_leased_against_the_reviewed_value(events in vec(event(), 0..60)) {
        run(events, |before, event, after, _| {
            let Event::PushRequested { to, force_with_lease, set_upstream } = event else {
                return Ok(());
            };
            let known = before
                .snapshot
                .disk()
                .and_then(|disk| disk.refs.refs.get(&to.tracking_ref()).cloned());
            let on_branch = matches!(
                before.snapshot.disk().map(|disk| &disk.head),
                Some(Head::Branch { .. })
            );
            let queued = after.ops.next.0 == before.ops.next.0 + 1;
            if !on_branch {
                prop_assert!(!queued, "pushed without a branch");
            } else if *force_with_lease && known.is_none() {
                prop_assert!(!queued, "forced without a remote value");
                let refused = matches!(
                    &after.notice,
                    Some(Notice::Refused { reason: Refusal::NoRemoteValue(_), .. })
                );
                prop_assert!(refused);
            } else {
                prop_assert!(queued);
                let request = after
                    .ops
                    .queued
                    .iter()
                    .map(|op| (op.id, &op.request))
                    .chain(after.ops.running.iter().map(|op| (op.id, &op.request)))
                    .find(|(id, _)| *id == before.ops.next)
                    .map(|(_, request)| request.clone());
                // Dispatch may already have refused it (lock, branch moved).
                let Some(request) = request else { return Ok(()) };
                let WriteRequest::Push { lease, to: pushed, set_upstream: up, .. } = request
                else {
                    return Err(TestCaseError::fail("not a push request"));
                };
                prop_assert_eq!(&pushed, to);
                prop_assert_eq!(up, *set_upstream);
                match (*force_with_lease, lease) {
                    (true, Some(lease)) => {
                        prop_assert_eq!(Some(lease.expected), known);
                        prop_assert_eq!(lease.remote_ref, to.remote_ref());
                    }
                    (false, None) => {}
                    other => return Err(TestCaseError::fail(format!("{other:?}"))),
                }
            }
            Ok(())
        })?;
    }

    /// Push and pull take an undo marker; fetch and set-upstream do not (they
    /// move nothing an undo could restore). Pull and push only start on
    /// the branch they were asked on, and pull only on a clean repo.
    #[test]
    fn network_commands_are_well_formed(events in vec(event(), 0..80)) {
        run(events, |before, _, _, commands| {
            let points = commands
                .iter()
                .filter(|command| matches!(command, Command::SnapshotForUndo(_)))
                .count();
            for command in commands {
                let Command::RunGit { op, args, expect } = command else { continue };
                match args {
                    GitArgs::Fetch { .. } | GitArgs::SetUpstream { .. } => {
                        prop_assert_eq!(points, 0)
                    }
                    GitArgs::Push { .. } | GitArgs::Pull { .. } => prop_assert_eq!(points, 1),
                    _ => {}
                }
                let Some(queued) = before.ops.queued.iter().find(|queued| queued.id == *op) else {
                    continue;
                };
                if let (WriteRequest::Pull { local, .. }, GitArgs::Pull { .. })
                | (WriteRequest::Push { local, .. }, GitArgs::Push { .. }) =
                    (&queued.request, args)
                {
                    let on = matches!(&expect.head, Head::Branch { name, .. } if name == local);
                    prop_assert!(on, "{expect:?} vs {local:?}");
                }
                if matches!(args, GitArgs::Pull { .. }) {
                    let clean = before
                        .snapshot
                        .disk()
                        .is_some_and(|disk| disk.phase == Phase::Clean);
                    prop_assert!(clean);
                }
            }
            Ok(())
        })?;
    }
}

fn oid_of(c: char) -> Oid {
    Oid::from_hex(&c.to_string().repeat(40)).unwrap()
}

fn read_disk(refs: &[(&str, char)], phase: Phase, head_oid: char) -> DiskState {
    DiskState {
        head: Head::Branch {
            name: BranchName("main".into()),
            oid: oid_of(head_oid),
        },
        phase,
        index: IndexId::Absent,
        status: StatusSnapshot::default(),
        refs: RefsSnapshot {
            refs: refs
                .iter()
                .map(|(name, c)| (RefName((*name).into()), oid_of(*c)))
                .collect(),
            ..RefsSnapshot::default()
        },
        lock: LockState::Free,
        stashes: Vec::new(),
    }
}

fn opened(disk: DiskState) -> RepoState {
    let (state, commands) = open(repo_id());
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    step(state, Event::StateRead { read: *read, disk }).0
}

fn origin_main() -> RemoteBranch {
    RemoteBranch {
        remote: "origin".into(),
        branch: BranchName("main".into()),
    }
}

fn push(force_with_lease: bool, set_upstream: bool) -> Event {
    Event::PushRequested {
        to: origin_main(),
        force_with_lease,
        set_upstream,
    }
}

#[test]
fn force_push_leases_the_snapshot_value() {
    let state = opened(read_disk(
        &[("refs/remotes/origin/main", 'b')],
        Phase::Clean,
        'a',
    ));
    let (state, commands) = step(state, push(true, false));
    let Some(Command::RunGit { args, .. }) = commands.last() else {
        panic!("{commands:?}");
    };
    assert_eq!(
        *args,
        GitArgs::Push {
            local: BranchName("main".into()),
            to: origin_main(),
            lease: Some(Lease {
                remote_ref: RefName("refs/heads/main".into()),
                expected: oid_of('b'),
            }),
            set_upstream: false,
        }
    );
    assert!(matches!(
        commands.first(),
        Some(Command::SnapshotForUndo(_))
    ));
    assert!(state.ops.running.is_some());
}

#[test]
fn force_push_without_a_known_remote_value_is_refused() {
    let state = opened(read_disk(&[], Phase::Clean, 'a'));
    let (state, commands) = step(state, push(true, false));
    assert!(commands.is_empty());
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Push,
            reason: Refusal::NoRemoteValue(RefName("refs/remotes/origin/main".into()))
        })
    );
    // A plain push of a new branch does not need one.
    let (_, commands) = step(state, push(false, true));
    assert!(commands.iter().any(|command| matches!(
        command,
        Command::RunGit {
            args: GitArgs::Push {
                lease: None,
                set_upstream: true,
                ..
            },
            ..
        }
    )));
}

#[test]
fn network_ops_need_a_branch_and_a_read() {
    let (state, _) = open(repo_id());
    let (state, _) = step(
        state,
        Event::PullRequested {
            from: origin_main(),
            mode: PullMode::Merge,
        },
    );
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Pull,
            reason: Refusal::NotRead
        })
    );
    // Fetch needs no branch: it is queued and waits for the first read.
    let (state, commands) = step(
        state,
        Event::FetchRequested {
            remote: "origin".into(),
        },
    );
    assert!(commands.is_empty());
    assert_eq!(state.ops.queued.len(), 1);

    let mut disk = read_disk(&[], Phase::Clean, 'a');
    disk.head = Head::Detached(oid_of('a'));
    let state = opened(disk);
    let (state, _) = step(state, push(false, false));
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Push,
            reason: Refusal::NoBranch
        })
    );
}

#[test]
fn pull_is_refused_mid_merge_and_leaves_a_marker_when_done() {
    let busy = opened(read_disk(
        &[],
        Phase::Merging {
            conflicts: Conflicts::default(),
        },
        'a',
    ));
    let (busy, commands) = step(
        busy,
        Event::PullRequested {
            from: origin_main(),
            mode: PullMode::Rebase,
        },
    );
    assert!(commands.is_empty());
    assert_eq!(
        busy.notice,
        Some(Notice::Refused {
            action: Action::Pull,
            reason: Refusal::RepoBusy
        })
    );

    // On a clean repo the pull runs, and once it succeeds the undo stack
    // holds a marker an undo stops at.
    let state = opened(read_disk(&[], Phase::Clean, 'a'));
    let (state, commands) = step(
        state,
        Event::PullRequested {
            from: origin_main(),
            mode: PullMode::Merge,
        },
    );
    let op = state.ops.running.as_ref().unwrap().id;
    assert!(matches!(commands[0], Command::SnapshotForUndo(_)));
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
    let after = read_disk(&[], Phase::Clean, 'd');
    let (state, _) = step(
        state,
        Event::StateRead {
            read: *read,
            disk: after,
        },
    );
    let entry = state.undo.entries.back().expect("a marker");
    assert_eq!(entry.reversible, Reversible::No(Irreversible::Pull));
    let (state, _) = step(state, Event::UndoRequested);
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Undo,
            reason: Refusal::NotUndoable(Irreversible::Pull)
        })
    );
}

#[test]
fn a_rejected_push_is_an_expected_outcome_not_an_error() {
    let state = opened(read_disk(&[], Phase::Clean, 'a'));
    let (state, _) = step(state, push(false, false));
    let op = state.ops.running.as_ref().unwrap().id;
    let failure = Failure {
        class: ErrorClass::Expected,
        details: "[rejected]".into(),
    };
    let (state, _) = step(
        state,
        Event::OpFinished(op, OpOutcome::Failed(failure.clone())),
    );
    assert_eq!(state.notice, Some(Notice::PushRejected { op, failure }));
    // The failed push left no undo marker.
    assert!(state.undo.entries.is_empty());
    assert_eq!(state.undo.pending, UndoPending::Idle);
}

#[test]
fn a_queued_push_is_refused_when_the_branch_changed_meanwhile() {
    // The lock holds the push back; by the time it is free HEAD is elsewhere.
    let mut locked = read_disk(&[], Phase::Clean, 'a');
    locked.lock = LockState::HeldByOther;
    let state = opened(locked.clone());
    let (state, commands) = step(state, push(false, false));
    assert!(commands.is_empty());
    let (state, commands) = step(state, Event::DiskChanged(ChangeSet([Area::Head].into())));
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    let moved = DiskState {
        head: Head::Branch {
            name: BranchName("dev".into()),
            oid: oid_of('a'),
        },
        lock: LockState::Free,
        ..locked
    };
    let (state, commands) = step(
        state,
        Event::StateRead {
            read: *read,
            disk: moved,
        },
    );
    assert!(commands.is_empty(), "{commands:?}");
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Push,
            reason: Refusal::BranchChanged
        })
    );
}

#[test]
fn a_pushed_branch_leaves_a_marker() {
    let state = opened(read_disk(&[], Phase::Clean, 'a'));
    let (state, _) = step(state, push(false, false));
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
            disk: read_disk(&[], Phase::Clean, 'a'),
        },
    );
    assert_eq!(
        state.undo.entries.back().map(|entry| entry.reversible),
        Some(Reversible::No(Irreversible::Push))
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Task 17: the lock is removed only when the user asked, the snapshot
    /// shows a lock held by someone else, and none of our operations is
    /// running or waiting; otherwise the request is refused, never dropped.
    #[test]
    fn a_lock_is_removed_only_when_it_cannot_be_ours(events in vec(event(), 0..60)) {
        run(events, |before, event, after, commands| {
            let removes = commands.iter().any(|c| matches!(c, Command::RemoveStaleLock));
            if !matches!(event, Event::RemoveStaleLockRequested) {
                prop_assert!(!removes);
                return Ok(());
            }
            let held = matches!(
                &before.snapshot,
                Snapshot::Read { disk, .. } if disk.lock == LockState::HeldByOther
            );
            let idle = before.ops.running.is_none() && before.ops.queued.is_empty();
            prop_assert_eq!(removes, held && idle);
            if !removes {
                prop_assert_eq!(
                    after.notice.clone(),
                    Some(Notice::Refused {
                        action: Action::RemoveLock,
                        reason: Refusal::NoStaleLock
                    })
                );
            }
            Ok(())
        })?;
    }
}

#[test]
fn a_leftover_lock_is_removed_on_request_and_nothing_else_is() {
    let locked = |lock| {
        let mut disk = read_disk(&[], Phase::Clean, 'a');
        disk.lock = lock;
        opened(disk)
    };
    let (state, commands) = step(
        locked(LockState::HeldByOther),
        Event::RemoveStaleLockRequested,
    );
    assert_eq!(commands, vec![Command::RemoveStaleLock]);
    assert_eq!(state.notice, None);

    // A lock that is free, or that our own git holds, is not removed.
    for lock in [LockState::Free, LockState::HeldByUs] {
        let (state, commands) = step(locked(lock), Event::RemoveStaleLockRequested);
        assert!(commands.is_empty());
        assert_eq!(
            state.notice,
            Some(Notice::Refused {
                action: Action::RemoveLock,
                reason: Refusal::NoStaleLock
            })
        );
    }

    // Not while one of our operations waits (the lock might be its own).
    let (queued, _) = step(
        locked(LockState::HeldByOther),
        Event::FetchRequested {
            remote: "origin".into(),
        },
    );
    assert!(!queued.ops.queued.is_empty() || queued.ops.running.is_some());
    let (_, commands) = step(queued, Event::RemoveStaleLockRequested);
    assert!(commands.is_empty());
}
