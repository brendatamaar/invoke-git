//! The model-based harness (backlog task 24; `docs/spec/verification.md`,
//! "Model-based test harness"). It owns laws 4, 5, 6, 8, 9 and 11, checks
//! law 7 (discard) inside its sequences and also
//! checks law 12 (no write while `index.lock` is held) on the real shell.
//!
//! Each case is a random sequence of up to 40 user and outside actions. The
//! real core + shell run it on one temp repo and plain git runs it on a
//! twin; the two are compared after every step. A failing sequence is
//! shrunk by proptest and printed as plain git commands.
//!
//! `INVOKE_HARNESS_CASES` sets the number of sequences (default 256, the
//! nightly job uses 10000) and `INVOKE_HARNESS_STEPS` the longest sequence.
//! The sequences are split over two tests so both cores of a CI runner work.

use harness::model::{TestEditor, config, steps};
use proptest_state_machine::prop_state_machine;

struct Editor;
impl TestEditor for Editor {
    const PROGRAM: &'static str = env!("CARGO_BIN_EXE_harness-rebase-editor");
}
type Harness = harness::model::Harness<Editor>;

/// The first half of the sequences (the law-ownership table names this one).
mod first_half {
    use super::*;

    prop_state_machine! {
        #![proptest_config(config(2))]

        #[test]
        fn model_based_sequences(sequential 1..steps() => Harness);
    }
}

mod second_half {
    use super::*;

    prop_state_machine! {
        #![proptest_config(config(2))]

        #[test]
        fn model_based_sequences_second_half(sequential 1..steps() => Harness);
    }
}

#[test]
fn interactive_rebase_pauses_and_restores_the_twin() {
    use harness::content::{Change, Eol};
    use harness::transition::{RebaseEnd, RebaseShape, Transition};
    use harness::world::World;

    for (shape, end) in [
        (RebaseShape::Edit, RebaseEnd::Abort),
        (RebaseShape::Reorder, RebaseEnd::Abort),
        (RebaseShape::Reorder, RebaseEnd::Continue),
        (RebaseShape::Reorder, RebaseEnd::Skip),
    ] {
        let mut world = World::with_rebase_editor(Some(std::path::Path::new(Editor::PROGRAM)));
        for text in ["first", "second"] {
            world.step(Transition::WriteFile {
                file: 0,
                change: Change::Whole {
                    lines: vec![text.into()],
                    eol: Eol::Lf,
                    final_newline: true,
                },
            });
            world.step(Transition::OutsideCommitAll {
                message: text.into(),
            });
        }
        world.step(Transition::WhileLocked(Box::new(
            Transition::InteractiveRebase {
                count: 2,
                shape,
                end,
            },
        )));
        assert_eq!(
            world.twin.git(&["log", "-1", "--format=%s"]).text(),
            "second"
        );
        assert_eq!(
            world.twin.git(&["diff", "--cached", "--name-only"]).text(),
            ""
        );
    }
}

#[test]
fn cherry_pick_and_rebase_share_the_same_model_history() {
    use harness::content::{Change, Eol};
    use harness::transition::{RebaseEnd, RebaseShape, ReplayEnd, Transition};
    use harness::world::World;

    let mut world = World::with_rebase_editor(Some(std::path::Path::new(Editor::PROGRAM)));
    let write = |text: &str| Transition::WriteFile {
        file: 0,
        change: Change::Whole {
            lines: vec![text.into()],
            eol: Eol::Lf,
            final_newline: true,
        },
    };
    world.step(write("main"));
    world.step(Transition::OutsideCommitAll {
        message: "main change".into(),
    });
    world.step(Transition::CreateBranch {
        name: 1,
        switch: true,
        start_back: true,
    });
    world.step(write("feat"));
    world.step(Transition::OutsideCommitAll {
        message: "feat change".into(),
    });
    let main = world.twin.git(&["rev-parse", "main"]).text();
    let commits = world.twin.git(&["rev-list", "--branches", "HEAD"]).text();
    let commit = commits.lines().position(|id| id == main).unwrap();
    world.step(Transition::WhileLocked(Box::new(Transition::Replay {
        commit,
        revert: false,
        end: ReplayEnd::Abort,
    })));
    world.step(Transition::InteractiveRebase {
        count: 2,
        shape: RebaseShape::Edit,
        end: RebaseEnd::Abort,
    });
    assert_eq!(
        world.twin.git(&["log", "-1", "--format=%s"]).text(),
        "feat change"
    );
}
