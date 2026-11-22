//! Task 14: what the core decides about a commit or an amend. The message is
//! cleaned and checked when the request arrives; the rest is decided on the
//! fresh snapshot when it is dispatched (laws 8, 9, 12).

use invoke_core::*;
use proptest::prelude::*;

fn oid(n: u64) -> Oid {
    Oid::from_hex(&format!("{n:040x}")).unwrap()
}

fn main_branch() -> BranchName {
    BranchName("main".into())
}

fn repo_id() -> RepoId {
    RepoId {
        root_commit: None,
        remote_url: None,
    }
}

#[derive(Clone, Copy, Debug)]
enum HeadKind {
    Branch,
    Detached,
    Unborn,
}

#[derive(Clone, Copy, Debug)]
enum PhaseKind {
    Clean,
    /// Merging with every conflict resolved.
    MergingResolved,
    MergingConflicted,
    Rebasing,
    CherryPicking,
    Reverting,
}

fn disk(head: HeadKind, phase: PhaseKind, staged: bool, unstaged: bool) -> DiskState {
    let entries = [
        staged.then(|| StatusEntry {
            path: RepoPath("s.txt".into()),
            staged: Some(FileChange::Modified),
            unstaged: None,
        }),
        unstaged.then(|| StatusEntry {
            path: RepoPath("u.txt".into()),
            staged: None,
            unstaged: Some(FileChange::Modified),
        }),
    ];
    let conflicts = |paths: &[&str]| {
        Conflicts::from_parts(
            paths
                .iter()
                .map(|path| (RepoPath((*path).into()), ConflictKind::BothModified)),
            [],
        )
    };
    DiskState {
        head: match head {
            HeadKind::Branch => Head::Branch {
                name: main_branch(),
                oid: oid(1),
            },
            HeadKind::Detached => Head::Detached(oid(1)),
            HeadKind::Unborn => Head::Unborn(main_branch()),
        },
        phase: match phase {
            PhaseKind::Clean => Phase::Clean,
            PhaseKind::MergingResolved => Phase::Merging {
                conflicts: conflicts(&[]),
            },
            PhaseKind::MergingConflicted => Phase::Merging {
                conflicts: conflicts(&["c.txt"]),
            },
            PhaseKind::Rebasing => Phase::Rebasing {
                step: 1,
                total: 2,
                conflicts: conflicts(&[]),
            },
            PhaseKind::CherryPicking => Phase::CherryPicking {
                conflicts: conflicts(&[]),
            },
            PhaseKind::Reverting => Phase::Reverting {
                conflicts: conflicts(&[]),
            },
        },
        index: IndexId::Checksum(oid(2)),
        status: StatusSnapshot::from_entries(entries.into_iter().flatten()),
        refs: RefsSnapshot::default(),
        stashes: Vec::new(),
        lock: LockState::Free,
    }
}

/// A repo whose first read has landed.
fn ready(disk: DiskState) -> RepoState {
    let (state, commands) = open(repo_id());
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    let (state, _) = step(state, Event::StateRead { read: *read, disk });
    state
}

fn commit(state: RepoState, message: &str, amend: bool) -> (RepoState, Vec<Command>) {
    step(
        state,
        Event::CommitRequested {
            message: message.into(),
            amend,
        },
    )
}

fn refused(state: &RepoState) -> Option<&Refusal> {
    match &state.notice {
        Some(Notice::Refused {
            action: Action::Commit,
            reason,
        }) => Some(reason),
        _ => None,
    }
}

/// The commands of an accepted commit: the undo point and the git run.
fn accepted(commands: &[Command]) -> (&UndoPoint, &GitArgs) {
    match commands {
        [
            Command::SnapshotForUndo(point),
            Command::RunGit { args, .. },
        ] => (point, args),
        other => panic!("not accepted: {other:?}"),
    }
}

fn nothing_ran(state: &RepoState, commands: &[Command]) {
    assert!(commands.is_empty(), "{commands:?}");
    assert!(state.ops.running.is_none() && state.ops.queued.is_empty());
    assert_eq!(state.undo.pending, UndoPending::Idle);
}

#[test]
fn an_empty_or_blank_message_is_refused_with_its_reason() {
    for (message, problem) in [
        ("", MessageProblem::Empty),
        (" \r\n\t \n", MessageProblem::OnlyWhitespace),
        ("a\0b", MessageProblem::ContainsNul),
    ] {
        for amend in [false, true] {
            let state = ready(disk(HeadKind::Branch, PhaseKind::Clean, true, false));
            let (state, commands) = commit(state, message, amend);
            nothing_ran(&state, &commands);
            assert_eq!(
                refused(&state),
                Some(&Refusal::Commit(CommitRefusal::Message(problem))),
                "{message:?}"
            );
            // Nothing was queued, so no op id was used up.
            assert_eq!(state.ops.next, OpId(0));
        }
    }
}

#[test]
fn nothing_staged_is_refused_and_unstaged_changes_do_not_count() {
    for unstaged in [false, true] {
        let state = ready(disk(HeadKind::Branch, PhaseKind::Clean, false, unstaged));
        let (state, commands) = commit(state, "Fix", false);
        nothing_ran(&state, &commands);
        assert_eq!(
            refused(&state),
            Some(&Refusal::Commit(CommitRefusal::NothingStaged))
        );
    }
}

#[test]
fn amend_needs_nothing_staged_but_a_commit_to_amend() {
    // A reword: nothing staged is fine.
    let state = ready(disk(HeadKind::Branch, PhaseKind::Clean, false, false));
    let (_, commands) = commit(state, "Better words", true);
    let (point, args) = accepted(&commands);
    assert_eq!(point.label, "Amend 'Better words'");
    assert_eq!(point.reverse, Reversal::Refs);
    assert_eq!(
        args,
        &GitArgs::Commit {
            message: "Better words\n".into(),
            amend: true
        }
    );
    // The branch is what an undo puts back (law 8), and only it (law 9).
    assert_eq!(
        point.refs,
        [RefValue {
            name: main_branch().to_ref(),
            oid: Some(oid(1))
        }]
    );

    // An unborn branch has nothing to amend, staged or not.
    for staged in [false, true] {
        let state = ready(disk(HeadKind::Unborn, PhaseKind::Clean, staged, false));
        let (state, commands) = commit(state, "x", true);
        nothing_ran(&state, &commands);
        assert_eq!(
            refused(&state),
            Some(&Refusal::Commit(CommitRefusal::AmendUnborn))
        );
    }

    // A detached HEAD can be amended: the point records `HEAD` itself.
    let state = ready(disk(HeadKind::Detached, PhaseKind::Clean, false, false));
    let (_, commands) = commit(state, "x", true);
    let (point, _) = accepted(&commands);
    assert_eq!(point.head, HeadTarget::Detached);
    assert_eq!(point.refs[0].name, RefName("HEAD".into()));
}

#[test]
fn the_first_commit_of_a_repo_records_a_branch_that_did_not_exist() {
    let state = ready(disk(HeadKind::Unborn, PhaseKind::Clean, true, false));
    let (_, commands) = commit(state, "First", false);
    let (point, _) = accepted(&commands);
    assert_eq!(point.label, "Commit 'First'");
    assert_eq!(
        point.refs,
        [RefValue {
            name: main_branch().to_ref(),
            oid: None
        }]
    );
}

#[test]
fn a_resolved_merge_is_concluded_by_a_commit_even_with_nothing_new_staged() {
    let state = ready(disk(
        HeadKind::Branch,
        PhaseKind::MergingResolved,
        false,
        false,
    ));
    let (state, commands) = commit(state, "Merge branch 'x'", false);
    let (point, _) = accepted(&commands);
    assert_eq!(point.label, "Conclude merge 'Merge branch 'x''");
    assert_eq!(point.reverse, Reversal::Never(Irreversible::MergeConcluded));
    let op = point.op;

    // Run it to the end: an undo then stops at the merge with its reason
    // instead of leaving the merge's changes staged without `MERGE_HEAD`.
    let (state, _) = step(state, Event::UndoSnapshotTaken { op, tree: oid(50) });
    let (state, commands) = step(state, Event::OpFinished(op, OpOutcome::Succeeded));
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    let mut after = disk(HeadKind::Branch, PhaseKind::Clean, false, false);
    after.head = Head::Branch {
        name: main_branch(),
        oid: oid(3),
    };
    after.index = IndexId::Checksum(oid(4));
    after.refs.refs.insert(main_branch().to_ref(), oid(3));
    let (state, _) = step(
        state,
        Event::StateRead {
            read: *read,
            disk: after,
        },
    );
    assert_eq!(state.undo.entries.len(), 1);
    let (state, commands) = step(state, Event::UndoRequested);
    assert!(commands.is_empty(), "{commands:?}");
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Undo,
            reason: Refusal::NotUndoable(Irreversible::MergeConcluded)
        })
    );
}

#[test]
fn a_merge_with_conflicts_or_an_amend_during_a_merge_is_refused() {
    let state = ready(disk(
        HeadKind::Branch,
        PhaseKind::MergingConflicted,
        true,
        false,
    ));
    let (state, commands) = commit(state, "Merge", false);
    nothing_ran(&state, &commands);
    assert_eq!(
        refused(&state),
        Some(&Refusal::Commit(CommitRefusal::UnresolvedConflicts))
    );

    let state = ready(disk(
        HeadKind::Branch,
        PhaseKind::MergingResolved,
        true,
        false,
    ));
    let (state, commands) = commit(state, "Merge", true);
    nothing_ran(&state, &commands);
    assert_eq!(
        refused(&state),
        Some(&Refusal::Commit(CommitRefusal::AmendDuringMerge))
    );
}

#[test]
fn rebase_cherry_pick_and_revert_belong_to_their_own_continue() {
    for phase in [
        PhaseKind::Rebasing,
        PhaseKind::CherryPicking,
        PhaseKind::Reverting,
    ] {
        for amend in [false, true] {
            let state = ready(disk(HeadKind::Branch, phase, true, true));
            let (state, commands) = commit(state, "x", amend);
            nothing_ran(&state, &commands);
            assert_eq!(refused(&state), Some(&Refusal::RepoBusy), "{phase:?}");
        }
    }
}

#[test]
fn a_commit_waits_for_a_fresh_snapshot_and_is_judged_on_it() {
    // Asked before anything was read: queued, nothing decided yet.
    let (state, commands) = open(repo_id());
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    let (state, commands) = commit(state, "Fix", false);
    assert!(commands.is_empty());
    assert_eq!(state.ops.queued.len(), 1);
    // The read shows nothing staged: now it is refused, with a reason.
    let (state, commands) = step(
        state,
        Event::StateRead {
            read: *read,
            disk: disk(HeadKind::Branch, PhaseKind::Clean, false, true),
        },
    );
    nothing_ran(&state, &commands);
    assert_eq!(
        refused(&state),
        Some(&Refusal::Commit(CommitRefusal::NothingStaged))
    );
}

fn head_kind() -> impl Strategy<Value = HeadKind> {
    prop_oneof![
        Just(HeadKind::Branch),
        Just(HeadKind::Detached),
        Just(HeadKind::Unborn)
    ]
}

fn phase_kind() -> impl Strategy<Value = PhaseKind> {
    prop_oneof![
        4 => Just(PhaseKind::Clean),
        2 => Just(PhaseKind::MergingResolved),
        1 => Just(PhaseKind::MergingConflicted),
        1 => Just(PhaseKind::Rebasing),
        1 => Just(PhaseKind::CherryPicking),
        1 => Just(PhaseKind::Reverting),
    ]
}

/// Messages with every kind of line break, blank lines and blank messages.
fn message() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[a-z日 \\t\\r\\n#]{0,30}",
        1 => Just(String::new()),
        1 => Just(" \r\n ".to_owned()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Every request ends one way: refused with a stated reason, or run
    /// with the cleaned message. The decision matches an independent
    /// statement of the rules, and git never gets a `\r`.
    #[test]
    fn a_commit_is_refused_or_run_exactly_as_the_rules_say(
        head in head_kind(),
        phase in phase_kind(),
        staged in any::<bool>(),
        unstaged in any::<bool>(),
        amend in any::<bool>(),
        text in message(),
    ) {
        let state = ready(disk(head, phase, staged, unstaged));
        let (state, commands) = commit(state, &text, amend);

        let blank = text.chars().all(|c| c.is_ascii_whitespace() || c == '\x0b');
        let expected: Result<&str, Refusal> = if blank {
            Err(Refusal::Commit(CommitRefusal::Message(if text.is_empty() {
                MessageProblem::Empty
            } else {
                MessageProblem::OnlyWhitespace
            })))
        } else {
            match (phase, amend) {
                (PhaseKind::Clean, true) if matches!(head, HeadKind::Unborn) => {
                    Err(Refusal::Commit(CommitRefusal::AmendUnborn))
                }
                (PhaseKind::Clean, true) => Ok("Amend"),
                (PhaseKind::Clean, false) if !staged => {
                    Err(Refusal::Commit(CommitRefusal::NothingStaged))
                }
                (PhaseKind::Clean, false) => Ok("Commit"),
                (PhaseKind::MergingResolved | PhaseKind::MergingConflicted, true) => {
                    Err(Refusal::Commit(CommitRefusal::AmendDuringMerge))
                }
                (PhaseKind::MergingConflicted, false) => {
                    Err(Refusal::Commit(CommitRefusal::UnresolvedConflicts))
                }
                (PhaseKind::MergingResolved, false) => Ok("Conclude merge"),
                (PhaseKind::Rebasing | PhaseKind::CherryPicking | PhaseKind::Reverting, _) => {
                    Err(Refusal::RepoBusy)
                }
            }
        };
        match expected {
            Err(reason) => {
                prop_assert!(commands.is_empty(), "{commands:?}");
                prop_assert_eq!(refused(&state), Some(&reason));
                prop_assert!(state.ops.running.is_none() && state.ops.queued.is_empty());
            }
            Ok(verb) => {
                let (point, args) = accepted(&commands);
                prop_assert!(point.label.starts_with(&format!("{verb} '")), "{}", point.label);
                prop_assert!(!point.label.contains('\n') && !point.label.contains('\r'));
                let GitArgs::Commit { message, amend: sent } = args else {
                    panic!("{args:?}");
                };
                prop_assert_eq!(*sent, amend);
                prop_assert!(!message.contains('\r'));
                prop_assert!(message.ends_with('\n') && !message.ends_with("\n\n"));
                prop_assert_eq!(message, &invoke_core::message::normalize(&text));
                // Only the branch (or HEAD) the commit moves is recorded.
                prop_assert_eq!(point.refs.len(), 1);
                prop_assert!(state.ops.running.is_some());
            }
        }
    }
}
