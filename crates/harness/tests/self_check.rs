//! The harness checks itself: a scripted sequence runs clean, a repo that
//! drifts from its twin is found, and the failure prints the twin's history
//! as plain git commands.

use std::panic::{AssertUnwindSafe, catch_unwind};

use harness::content::{Change, Eol, LineEdit};
use harness::transition::{BranchSel, Pick, Target, Transition};
use harness::world::World;

fn edit(file: usize, at: usize, text: &str) -> Transition {
    Transition::WriteFile {
        file,
        change: Change::Edits {
            edits: vec![LineEdit::Replace {
                at,
                text: text.into(),
            }],
            eol: Eol::Lf,
            final_newline: None,
        },
    }
}

fn message_of(error: Box<dyn std::any::Any + Send>) -> String {
    error
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| error.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_default()
}

#[test]
fn a_scripted_sequence_of_user_and_outside_actions_runs_clean() {
    let mut world = World::new();
    for transition in [
        edit(0, 2, "changed two"),
        edit(0, 20, "changed twenty"),
        Transition::Stage {
            file: 0,
            pick: Pick::Hunk(1),
        },
        Transition::Stage {
            file: 0,
            pick: Pick::Lines { hunk: 0, mask: 1 },
        },
        Transition::Commit {
            message: "first".into(),
            amend: false,
        },
        Transition::CreateBranch {
            name: 1,
            switch: true,
            start_back: false,
        },
        Transition::StageEveryHunk { file: 0 },
        Transition::WhileLocked(Box::new(Transition::Commit {
            message: "locked".into(),
            amend: false,
        })),
        Transition::OutsideCheckout {
            target: Target::Branch(BranchSel::Pool(0)),
        },
        Transition::Undo,
        Transition::StashSave {
            include_untracked: true,
            message: "wip".into(),
        },
        Transition::StashDrop { entry: 0 },
        Transition::Undo,
        Transition::OutsideCommitAll {
            message: "outside".into(),
        },
        Transition::Undo,
    ] {
        world.step(transition);
    }
}

#[test]
fn a_repo_that_drifts_from_its_twin_is_found_and_replayed_as_git_commands() {
    let mut world = World::new();
    world.step(edit(0, 3, "three"));
    world.step(Transition::Stage {
        file: 0,
        pick: Pick::File,
    });
    // Something changes the app repo only: the two no longer agree.
    assert!(world.app.repo.git(&["branch", "drifted"]).ok);
    let failure = catch_unwind(AssertUnwindSafe(|| world.step(Transition::Refresh)))
        .expect_err("the drift must be found");
    let message = message_of(failure);
    assert!(message.contains("refs differ"), "{message}");
    assert!(message.contains("drifted"), "{message}");
    // The replay is plain git: the twin's history, in order.
    assert!(message.contains("git init -q -b main twin"), "{message}");
    assert!(message.contains("git add -- a.txt"), "{message}");
    assert!(message.contains("# --- step 2: Stage"), "{message}");
}

#[test]
fn a_file_the_app_changed_behind_the_twin_is_found() {
    let mut world = World::new();
    world.app.repo.write("a.txt", "someone else\n");
    let failure = catch_unwind(AssertUnwindSafe(|| world.step(Transition::Refresh)))
        .expect_err("the drift must be found");
    assert!(message_of(failure).contains("worktree files differ"));
}

fn empty_file(file: usize) -> Transition {
    Transition::WriteFile {
        file,
        change: Change::Whole {
            lines: vec![],
            eol: Eol::Lf,
            final_newline: false,
        },
    }
}

/// `git switch -c topic/x` checks the files out and only then finds that
/// the branch `topic` is in the way; the app refuses up front and changes
/// nothing, so the twin must be asked on a copy.
#[test]
fn a_refusal_the_app_makes_before_git_would_half_run_is_not_a_difference() {
    let mut world = World::new();
    for transition in [
        empty_file(0),
        Transition::CreateBranch {
            name: 2,
            switch: false,
            start_back: false,
        },
        Transition::Stage {
            file: 0,
            pick: Pick::File,
        },
        empty_file(0),
        Transition::OutsideCommitAll {
            message: "a 0".into(),
        },
        Transition::CreateBranch {
            name: 3,
            switch: true,
            start_back: true,
        },
    ] {
        world.step(transition);
    }
}

/// Undoing a branch switch is a real checkout; with a local change in the
/// way git refuses and so does the app, which reports it as expected.
#[test]
fn an_undo_that_git_refuses_to_check_out_is_agreement() {
    let mut world = World::new();
    for transition in [
        Transition::CreateBranch {
            name: 2,
            switch: false,
            start_back: false,
        },
        empty_file(1),
        Transition::OutsideCommitAll {
            message: "a 0".into(),
        },
        Transition::SwitchBranch {
            name: BranchSel::Pool(2),
        },
        empty_file(1),
        Transition::Undo,
    ] {
        world.step(transition);
    }
}
