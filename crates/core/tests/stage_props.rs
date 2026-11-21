//! Backlog task 11, core half: stage and unstage go through the op queue
//! (fresh snapshot, free lock, expected index id, undo point), and the
//! status snapshot is a sorted tree-friendly list.

use invoke_core::*;
use proptest::collection::vec;
use proptest::prelude::*;

fn hex(c: char) -> Oid {
    Oid::from_hex(&c.to_string().repeat(40)).unwrap()
}

fn index(c: char) -> IndexId {
    IndexId::Checksum(hex(c))
}

fn disk(index: IndexId, lock: LockState) -> DiskState {
    DiskState {
        head: Head::Branch {
            name: BranchName("main".into()),
            oid: hex('a'),
        },
        phase: Phase::Clean,
        index,
        status: StatusSnapshot::default(),
        refs: RefsSnapshot {
            refs: [(RefName("refs/heads/main".into()), hex('a'))].into(),
            ..RefsSnapshot::default()
        },
        lock,
    }
}

fn request(path: &str, based_on: IndexId) -> StageRequest {
    StageRequest {
        selection: Selection::Lines {
            path: RepoPath(path.into()),
            hunk: 0,
            lines: vec![1, 2, 3],
        },
        patch: Patch("diff --git a/f b/f\n".into()),
        based_on,
    }
}

/// An opened repo whose first read (`disk`) has landed.
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

#[test]
fn stage_snapshots_then_applies_with_the_expected_index() {
    let state = ready(disk(index('b'), LockState::Free));
    let (state, commands) = step(
        state,
        Event::StageRequested(request("src/a.rs", index('b'))),
    );
    let op = state.ops.running.as_ref().expect("running").id;
    assert_eq!(
        commands,
        vec![
            Command::SnapshotForUndo(UndoPoint {
                op,
                label: "Stage 3 lines in src/a.rs".into(),
                head: HeadTarget::Branch(BranchName("main".into())),
                refs: Vec::new(),
                index: index('b'),
                index_effect: IndexEffect::Rewrites,
            }),
            Command::ApplyPatchToIndex {
                op,
                patch: Patch("diff --git a/f b/f\n".into()),
                expect_index: index('b'),
            },
        ]
    );
    assert!(commands.iter().all(Command::is_write));
}

#[test]
fn unstage_labels_its_undo_point() {
    let state = ready(disk(index('b'), LockState::Free));
    let mut req = request("a.rs", index('b'));
    req.selection = Selection::Hunk {
        path: RepoPath("a.rs".into()),
        hunk: 1,
    };
    let (_, commands) = step(state, Event::UnstageRequested(req));
    let Command::SnapshotForUndo(point) = &commands[0] else {
        panic!("{commands:?}");
    };
    assert_eq!(point.label, "Unstage hunk 2 in a.rs");
}

#[test]
fn a_patch_from_an_older_index_is_refused_and_nothing_runs() {
    let state = ready(disk(index('b'), LockState::Free));
    let (state, commands) = step(state, Event::StageRequested(request("a", index('c'))));
    assert!(commands.is_empty(), "{commands:?}");
    assert!(state.ops.running.is_none() && state.ops.queued.is_empty());
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Stage,
            reason: Refusal::StaleDiff
        })
    );
}

#[test]
fn an_empty_patch_is_refused_at_once() {
    let state = ready(disk(index('b'), LockState::Free));
    let mut req = request("a", index('b'));
    req.patch = Patch(String::new());
    let (state, commands) = step(state, Event::UnstageRequested(req));
    assert!(commands.is_empty());
    assert!(state.ops.queued.is_empty() && state.ops.running.is_none());
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Unstage,
            reason: Refusal::NothingSelected
        })
    );
}

#[test]
fn stage_waits_for_the_lock_then_runs_on_the_fresh_read() {
    let state = ready(disk(index('b'), LockState::HeldByOther));
    let (state, commands) = step(state, Event::StageRequested(request("a", index('b'))));
    assert!(commands.is_empty());
    assert_eq!(state.ops.queued.len(), 1);
    let (state, commands) = step(state, Event::RefreshRequested);
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    let (_, commands) = step(
        state,
        Event::StateRead {
            read: *read,
            disk: disk(index('b'), LockState::Free),
        },
    );
    assert!(matches!(
        commands.as_slice(),
        [
            Command::SnapshotForUndo(_),
            Command::ApplyPatchToIndex { .. }
        ]
    ));
}

#[test]
fn a_queued_stage_checks_the_index_it_will_really_run_on() {
    // The index moves while the stage waits: the fresh read has another id.
    let state = ready(disk(index('b'), LockState::HeldByOther));
    let (state, _) = step(state, Event::StageRequested(request("a", index('b'))));
    let (state, commands) = step(state, Event::RefreshRequested);
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    let (state, commands) = step(
        state,
        Event::StateRead {
            read: *read,
            disk: disk(index('c'), LockState::Free),
        },
    );
    assert!(commands.is_empty());
    assert!(state.ops.queued.is_empty() && state.ops.running.is_none());
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::Stage,
            reason: Refusal::StaleDiff
        })
    );
}

#[test]
fn a_finished_stage_can_be_undone() {
    let state = ready(disk(index('b'), LockState::Free));
    let (state, _) = step(state, Event::StageRequested(request("a.rs", index('b'))));
    let op = state.ops.running.as_ref().unwrap().id;
    let (state, _) = step(state, Event::UndoSnapshotTaken { op, tree: hex('c') });
    let (state, commands) = step(state, Event::OpFinished(op, OpOutcome::Succeeded));
    let [Command::ReadState { read, .. }] = commands.as_slice() else {
        panic!("{commands:?}");
    };
    // The write changed the index id; refs and head stay.
    let (state, _) = step(
        state,
        Event::StateRead {
            read: *read,
            disk: disk(index('d'), LockState::Free),
        },
    );
    let entry = state.undo.entries.back().expect("an undo point");
    assert_eq!(entry.point.label, "Stage 3 lines in a.rs");
    assert_eq!(entry.after_index, index('d'));
    let (_, commands) = step(state, Event::UndoRequested);
    assert!(
        matches!(commands.as_slice(), [Command::ReplayUndo(replay)] if replay.tree == hex('c')),
        "{commands:?}"
    );
}

#[test]
fn a_failed_stage_leaves_no_undo_point() {
    let state = ready(disk(index('b'), LockState::Free));
    let (state, _) = step(state, Event::StageRequested(request("a.rs", index('b'))));
    let op = state.ops.running.as_ref().unwrap().id;
    let (state, _) = step(state, Event::UndoSnapshotTaken { op, tree: hex('c') });
    let failure = Failure {
        class: ErrorClass::PreconditionChanged,
        details: "index changed".into(),
    };
    let (state, commands) = step(state, Event::OpFinished(op, OpOutcome::Failed(failure)));
    assert!(
        commands.contains(&Command::PruneUndo(vec![op])),
        "{commands:?}"
    );
    assert!(state.undo.entries.is_empty());
}

proptest! {
    /// Whatever the disk looks like, a stage runs exactly when its diff
    /// was made against the index on disk, and then expects that index.
    #[test]
    fn a_stage_runs_iff_its_diff_matches_the_index(
        disk_index in prop_oneof![Just(IndexId::Absent), "[a-f]".prop_map(|c| index(c.chars().next().unwrap()))],
        based_on in prop_oneof![Just(IndexId::Absent), "[a-f]".prop_map(|c| index(c.chars().next().unwrap()))],
        unstage in any::<bool>(),
    ) {
        let state = ready(disk(disk_index.clone(), LockState::Free));
        let req = request("a", based_on.clone());
        let event = if unstage { Event::UnstageRequested(req) } else { Event::StageRequested(req) };
        let (state, commands) = step(state, event);
        if based_on == disk_index {
            let applied = commands.iter().any(|c| matches!(
                c,
                Command::ApplyPatchToIndex { expect_index, .. } if *expect_index == disk_index
            ));
            prop_assert!(applied, "{:?}", commands);
        } else {
            prop_assert!(commands.is_empty());
            prop_assert_eq!(
                state.notice,
                Some(Notice::Refused {
                    action: if unstage { Action::Unstage } else { Action::Stage },
                    reason: Refusal::StaleDiff,
                })
            );
        }
    }

    /// The status list is sorted by path, has one entry per path, keeps
    /// every change, and each directory is one contiguous run.
    #[test]
    fn status_snapshots_are_sorted_merged_and_tree_ordered(
        raw in vec(
            (
                "[a-c.\\-]{1,2}(/[a-c.\\-]{1,2}){0,2}",
                proptest::option::of(Just(FileChange::Modified)),
                proptest::option::of(Just(FileChange::Untracked)),
            ),
            0..12,
        )
    ) {
        let entries: Vec<StatusEntry> = raw
            .iter()
            .map(|(path, staged, unstaged)| StatusEntry {
                path: RepoPath(path.clone()),
                staged: staged.clone(),
                unstaged: unstaged.clone(),
            })
            .collect();
        let snapshot = StatusSnapshot::from_entries(entries);
        let paths: Vec<&str> = snapshot.entries.iter().map(|e| e.path.0.as_str()).collect();
        prop_assert!(paths.windows(2).all(|w| w[0] < w[1]), "{:?}", paths);
        for entry in &snapshot.entries {
            prop_assert!(entry.staged.is_some() || entry.unstaged.is_some());
        }
        for (path, _, _) in &raw {
            let staged_any = raw.iter().any(|(p, s, _)| p == path && s.is_some());
            let unstaged_any = raw.iter().any(|(p, _, u)| p == path && u.is_some());
            let found = snapshot.entries.iter().find(|e| e.path.0 == *path);
            prop_assert_eq!(found.is_some(), staged_any || unstaged_any);
            if let Some(found) = found {
                prop_assert_eq!(found.staged.is_some(), staged_any);
                prop_assert_eq!(found.unstaged.is_some(), unstaged_any);
            }
        }
        // Every directory prefix is contiguous.
        let mut closed: Vec<String> = Vec::new();
        let mut open_dirs: Vec<String> = Vec::new();
        for path in &paths {
            let mut dirs: Vec<String> = Vec::new();
            let mut at = 0;
            while let Some(slash) = path[at..].find('/') {
                at += slash + 1;
                dirs.push(path[..at].to_owned());
            }
            for gone in open_dirs.iter().filter(|d| !dirs.contains(d)).cloned().collect::<Vec<_>>() {
                closed.push(gone);
            }
            open_dirs = dirs;
            for dir in &open_dirs {
                prop_assert!(!closed.contains(dir), "{} reopened in {:?}", dir, paths);
            }
        }
    }
}
