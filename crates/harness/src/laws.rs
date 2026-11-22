//! The law-ownership table: for each of the 14 laws in `docs/PRD.md`, the
//! one test that owns it (`docs/spec/verification.md`, "Laws mapped to
//! tests"). [`check`] fails when an owning test no longer exists, so a law
//! cannot silently lose its test. A law whose backlog task is not merged yet
//! has a clearly named placeholder instead; the check fails the day the real
//! test appears under that name, so the entry is promoted.

use std::fs;
use std::path::Path;

/// A test function, by file (relative to the repo root) and name.
#[derive(Clone, Copy, Debug)]
pub struct TestRef {
    pub file: &'static str,
    pub name: &'static str,
}

/// Text that must appear in a file: where a model-based harness checks a
/// law inside one big test, the check is found by its message.
#[derive(Clone, Copy, Debug)]
pub struct Marker {
    pub file: &'static str,
    pub text: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub enum Owner {
    /// The law's owning test, which runs in CI today.
    Test(TestRef),
    /// The backlog task that adds the owner is not merged: the test that
    /// will own the law, by the name it must have.
    Pending { task: u8, will_be: TestRef },
}

#[derive(Clone, Copy, Debug)]
pub struct Law {
    pub number: u8,
    pub summary: &'static str,
    pub owner: Owner,
    /// Other tests that exercise the law on real git or at another level.
    pub also: &'static [TestRef],
    /// For laws a model-based sequence checks inline.
    pub markers: &'static [Marker],
}

const HARNESS_TEST: TestRef = TestRef {
    file: "crates/harness/tests/model_based.rs",
    name: "model_based_sequences",
};

const fn test(file: &'static str, name: &'static str) -> TestRef {
    TestRef { file, name }
}

const fn marker(text: &'static str) -> Marker {
    Marker {
        file: "crates/harness/src/world.rs",
        text,
    }
}

pub const LAWS: [Law; 14] = [
    Law {
        number: 1,
        summary: "apply(diff(a, b), a) == b",
        owner: Owner::Test(test(
            "crates/diff/tests/laws.rs",
            "law_1_apply_diff_gives_new_text",
        )),
        also: &[],
        markers: &[],
    },
    Law {
        number: 2,
        summary: "diff(a, a) is empty",
        owner: Owner::Test(test(
            "crates/diff/tests/laws.rs",
            "law_2_diff_of_same_text_is_empty",
        )),
        also: &[],
        markers: &[],
    },
    Law {
        number: 3,
        summary: "hunk line numbers map to real lines",
        owner: Owner::Test(test(
            "crates/diff/tests/laws.rs",
            "law_3_hunk_numbers_map_to_real_lines",
        )),
        also: &[],
        markers: &[],
    },
    Law {
        number: 4,
        summary: "staging never changes the worktree file",
        owner: Owner::Test(HARNESS_TEST),
        also: &[test(
            "crates/shell/tests/staging.rs",
            "law_4_and_5_every_hunk_staged",
        )],
        markers: &[marker("law 4:")],
    },
    Law {
        number: 5,
        summary: "staging every hunk makes index = worktree",
        owner: Owner::Test(HARNESS_TEST),
        also: &[test(
            "crates/shell/tests/staging.rs",
            "law_4_and_5_every_hunk_staged",
        )],
        markers: &[marker("law 5:")],
    },
    Law {
        number: 6,
        summary: "stage then unstage restores the index",
        owner: Owner::Test(HARNESS_TEST),
        also: &[test(
            "crates/shell/tests/staging.rs",
            "law_6_stage_then_unstage_restores_the_index",
        )],
        markers: &[marker("law 6:")],
    },
    Law {
        number: 7,
        summary: "discard changes only the selected lines",
        // Discard is backlog task 13 and is not merged yet.
        owner: Owner::Pending {
            task: 13,
            will_be: test(
                "crates/shell/tests/discard.rs",
                "law_7_discard_changes_only_selected_lines",
            ),
        },
        also: &[],
        markers: &[],
    },
    Law {
        number: 8,
        summary: "undo restores refs and index exactly",
        owner: Owner::Test(HARNESS_TEST),
        also: &[
            test(
                "crates/shell/tests/undo.rs",
                "undo_commit_restores_head_branch_and_index",
            ),
            test("crates/core/tests/undo_props.rs", "undo_matches_the_oracle"),
            // Task 23: an abort puts everything back exactly.
            test(
                "crates/shell/tests/conflicts.rs",
                "aborting_a_merge_restores_head_branch_and_index_exactly",
            ),
            test(
                "crates/shell/tests/conflicts.rs",
                "aborting_a_rebase_restores_the_branch_head_and_index_exactly",
            ),
        ],
        markers: &[marker("law 8/9:")],
    },
    Law {
        number: 9,
        summary: "no untargeted branch moves",
        owner: Owner::Test(HARNESS_TEST),
        also: &[
            test("crates/shell/tests/undo.rs", "undo_moves_no_other_branch"),
            test(
                "crates/core/tests/branch_props.rs",
                "branches_match_the_oracle",
            ),
            // Task 23: finishing a merge or rebase moves only its branch.
            test(
                "crates/shell/tests/conflicts.rs",
                "rebase_advances_step_by_step_with_continue_and_skip",
            ),
            test(
                "crates/core/tests/conflict_props.rs",
                "conflict_runs_name_no_branch_and_expect_the_snapshot",
            ),
        ],
        markers: &[marker("law 8/9:")],
    },
    Law {
        number: 10,
        summary: "force push only with lease on the last fetched value",
        // Spec: a unit test on command building plus an integration test
        // with a local bare remote (task 17).
        owner: Owner::Test(test(
            "crates/shell/src/git.rs",
            "force_only_ever_comes_with_an_expected_value",
        )),
        also: &[
            test(
                "crates/shell/tests/sync.rs",
                "force_with_lease_overwrites_only_the_reviewed_value",
            ),
            test(
                "crates/shell/tests/sync.rs",
                "force_with_lease_is_refused_when_the_remote_moved_since_review",
            ),
            test(
                "crates/core/tests/step_props.rs",
                "force_push_is_leased_against_the_reviewed_value",
            ),
        ],
        markers: &[],
    },
    Law {
        number: 11,
        summary: "app state = state derived from disk",
        owner: Owner::Test(HARNESS_TEST),
        also: &[test(
            "crates/core/tests/step_props.rs",
            "disk_changes_only_request_a_read",
        )],
        markers: &[marker("law 11:")],
    },
    Law {
        number: 12,
        summary: "no write while another process holds index.lock",
        owner: Owner::Test(test(
            "crates/shell/tests/runner.rs",
            "held_index_lock_is_refused_and_left_alone",
        )),
        also: &[
            HARNESS_TEST,
            test(
                "crates/core/tests/step_props.rs",
                "writes_need_a_fresh_unlocked_snapshot",
            ),
            // Task 23: the conflict flow waits for the lock too.
            test(
                "crates/core/tests/conflict_props.rs",
                "conflict_writes_wait_for_a_fresh_unlocked_snapshot",
            ),
            test(
                "crates/shell/tests/conflicts.rs",
                "a_lock_held_by_another_process_holds_the_abort_back",
            ),
        ],
        markers: &[marker("law 12:")],
    },
    Law {
        number: 13,
        summary: "a cancelled operation leaves the repo unchanged or resumable",
        owner: Owner::Test(test(
            "crates/shell/tests/runner.rs",
            "cancel_kills_the_hook_and_leaves_head_alone",
        )),
        also: &[
            test(
                "crates/shell/tests/runner.rs",
                "cancel_right_after_start_is_cancelled",
            ),
            // Task 23: a running abort or continue is never killed half way.
            test(
                "crates/core/tests/conflict_props.rs",
                "a_running_conflict_operation_cannot_be_cancelled",
            ),
        ],
        markers: &[],
    },
    Law {
        number: 14,
        summary: "every commit once, edges point down",
        owner: Owner::Test(test(
            "crates/graph/tests/layout_props.rs",
            "edges_point_down_to_the_parent",
        )),
        also: &[],
        markers: &[],
    },
];

/// Looks for `fn <name>(` in `source`; true when it is a test (a `#[test]`
/// just above it, or inside a `proptest!` block) and not ignored.
fn is_live_test(source: &str, name: &str) -> bool {
    let needle = format!("fn {name}(");
    let lines: Vec<&str> = source.lines().collect();
    let Some(at) = lines.iter().position(|line| {
        line.contains(&needle)
            && line
                .find(&needle)
                .is_none_or(|at| at == 0 || !line.as_bytes()[at - 1].is_ascii_alphanumeric())
    }) else {
        return false;
    };
    let above = &lines[at.saturating_sub(6)..at];
    if above.iter().any(|line| line.contains("#[ignore")) {
        return false;
    }
    let attribute = above.iter().any(|line| line.trim() == "#[test]");
    let in_proptest = lines[..at]
        .iter()
        .any(|line| line.contains("proptest!") || line.contains("prop_state_machine!"));
    attribute || in_proptest
}

fn read(root: &Path, file: &str) -> Option<String> {
    fs::read_to_string(root.join(file)).ok()
}

/// What the check found: `errors` fail the build, `notes` list the laws
/// still waiting for their backlog task.
#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub notes: Vec<String>,
}

/// Checks `laws` against the source tree under `root`.
pub fn check_laws(root: &Path, laws: &[Law]) -> Report {
    let mut report = Report::default();
    let numbers: Vec<u8> = laws.iter().map(|law| law.number).collect();
    let expected: Vec<u8> = (1..=14).collect();
    if numbers != expected {
        report.errors.push(format!(
            "the table must list laws 1 to 14 in order, it has {numbers:?}"
        ));
    }
    for law in laws {
        let at = format!("law {}", law.number);
        let live = |test: &TestRef, report: &mut Report, what: &str| match read(root, test.file) {
            None => report.errors.push(format!(
                "{at}: {what} {} is in {}, which does not exist",
                test.name, test.file
            )),
            Some(source) if !is_live_test(&source, test.name) => report.errors.push(format!(
                "{at}: {what} {} is no longer a running test in {}",
                test.name, test.file
            )),
            Some(_) => {}
        };
        match &law.owner {
            Owner::Test(owner) => live(owner, &mut report, "owning test"),
            Owner::Pending { task, will_be } => {
                let present = read(root, will_be.file)
                    .is_some_and(|source| is_live_test(&source, will_be.name));
                if present {
                    report.errors.push(format!(
                        "{at}: task {task} has landed its test {} in {}; make it the owner",
                        will_be.name, will_be.file
                    ));
                } else {
                    report.notes.push(format!(
                        "{at} ({}) waits for task {task}: {} in {}",
                        law.summary, will_be.name, will_be.file
                    ));
                }
            }
        }
        for other in law.also {
            live(other, &mut report, "supporting test");
        }
        for marker in law.markers {
            match read(root, marker.file) {
                Some(source) if source.contains(marker.text) => {}
                _ => report.errors.push(format!(
                    "{at}: the check {:?} is gone from {}",
                    marker.text, marker.file
                )),
            }
        }
    }
    report
}

/// The numbered laws under "## Laws" in the PRD and the rows of the table in
/// the verification spec: both must still say 14.
pub fn check_docs(root: &Path) -> Vec<String> {
    let mut errors = Vec::new();
    match read(root, "docs/PRD.md") {
        Some(prd) => {
            let section = prd
                .split("\n## Laws")
                .nth(1)
                .and_then(|rest| rest.split("\n## ").next())
                .unwrap_or("");
            let numbers: Vec<u8> = section
                .lines()
                .filter_map(|line| line.split_once(". ")?.0.parse().ok())
                .collect();
            if numbers != (1..=14).collect::<Vec<u8>>() {
                errors.push(format!(
                    "docs/PRD.md lists laws {numbers:?}; add the new law to crates/harness/src/laws.rs"
                ));
            }
        }
        None => errors.push("docs/PRD.md is missing".into()),
    }
    match read(root, "docs/spec/verification.md") {
        Some(spec) => {
            let rows: Vec<u8> = spec
                .lines()
                .filter_map(|line| line.strip_prefix("| ")?.split(" |").next()?.parse().ok())
                .collect();
            if rows != (1..=14).collect::<Vec<u8>>() {
                errors.push(format!(
                    "docs/spec/verification.md maps laws {rows:?}, not 1 to 14"
                ));
            }
        }
        None => errors.push("docs/spec/verification.md is missing".into()),
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ignored_or_missing_test_is_not_live() {
        let source = "#[test]\nfn alive() {}\n\n#[test]\n#[ignore]\nfn off() {}\n";
        assert!(is_live_test(source, "alive"));
        assert!(!is_live_test(source, "off"));
        assert!(!is_live_test(source, "gone"));
        assert!(!is_live_test("fn not_a_test() {}\n", "not_a_test"));
        assert!(is_live_test(
            "proptest! {\n    #[test]\n    fn inside(x in 0..3) {}\n}\n",
            "inside"
        ));
        // A longer name does not count for a shorter one.
        assert!(!is_live_test("#[test]\nfn law_1_apply() {}\n", "apply"));
    }
}
