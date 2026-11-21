//! Turning a finished git process into the core's error classes, once, here
//! (`docs/spec/core-design.md` → "Errors"). git's messages are matched in
//! English, which `LANGUAGE=en` guarantees.

use invoke_core::{ErrorClass, Failure, OpOutcome};

/// Markers checked in order; the first class with a match wins. Auth comes
/// before network because `unable to access` also wraps auth failures.
const MARKERS: [(ErrorClass, &[&str]); 4] = [
    (
        ErrorClass::PreconditionChanged,
        &[
            "index.lock': File exists",
            "Unable to create '",
            "cannot lock ref",
            // The core checked the name was free; someone took it since.
            "a branch named '",
        ],
    ),
    (
        ErrorClass::Auth,
        &[
            "Authentication failed",
            "could not read Username",
            "could not read Password",
            "terminal prompts disabled",
            "Permission denied (publickey",
            "Host key verification failed",
        ],
    ),
    (
        ErrorClass::Network,
        &[
            "Could not resolve host",
            "Connection timed out",
            "Operation timed out",
            "Connection refused",
            "Failed to connect",
            "SSL certificate problem",
            "early EOF",
        ],
    ),
    (
        ErrorClass::Expected,
        &[
            "already exists and is not an empty directory",
            "does not appear to be a git repository",
            "does not exist",
            "Repository not found",
            "nothing to commit",
            "no changes added to commit",
            "nothing added to commit",
            "Aborting commit due to empty commit message",
            "[rejected]",
            "[remote rejected]",
            "non-fast-forward",
            // Task 17: pull and push.
            "stale info",
            "couldn't find remote ref",
            "cannot pull with rebase",
            "would be overwritten by merge",
            "Not possible to fast-forward",
            "CONFLICT",
            "Automatic merge failed",
            // Branches (task 15): git refuses before it changes anything.
            "is not fully merged",
            "would be overwritten",
            "Please commit your changes or stash them",
            "resolve your current index first",
            "used by worktree at",
            "checked out at",
            "is already checked out",
            "not a valid object name",
            "not a commit",
            "not a valid branch point",
            "reference is not a tree",
            "invalid reference",
            // Task 20: stash. A pop or apply that conflicts keeps the
            // stash and leaves the repo in git's own state; one that git
            // refuses (local changes in the way) changes nothing.
            "Your local changes to the following files would be overwritten",
            "already exists, no checkout",
            "could not restore untracked files from stash",
            "No stash entries found",
            "No local changes to save",
            "You do not have the initial commit yet",
        ],
    ),
];

/// Whether a failed `git branch -d` was refused because the branch holds
/// commits that are not merged anywhere else. The UI then offers a forced
/// delete (`DeleteBranch { force: true }`).
pub fn is_unmerged_refusal(details: &str) -> bool {
    details.contains("is not fully merged")
}

/// git's stdout and stderr together: some messages (`nothing to commit`)
/// go to stdout.
pub fn classify(success: bool, output: &str) -> OpOutcome {
    if success {
        return OpOutcome::Succeeded;
    }
    let class = MARKERS
        .iter()
        .find(|(_, markers)| markers.iter().any(|marker| output.contains(marker)))
        .map_or(ErrorClass::Bug, |(class, _)| *class);
    OpOutcome::Failed(Failure {
        class,
        details: output.to_owned(),
    })
}

/// `git stash push` with nothing to save exits 0 and prints `No local
/// changes to save`. No stash was made, so it is not a success.
pub fn classify_stash_save(success: bool, output: &str) -> OpOutcome {
    if success && output.contains("No local changes to save") {
        return classify(false, output);
    }
    classify(success, output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(output: &str) -> Option<ErrorClass> {
        match classify(false, output) {
            OpOutcome::Failed(failure) => Some(failure.class),
            _ => None,
        }
    }

    #[test]
    fn real_messages_classify() {
        use ErrorClass::*;
        let cases = [
            (
                "fatal: Unable to create '/r/.git/index.lock': File exists.\n\nAnother git process seems to be running",
                PreconditionChanged,
            ),
            (
                "error: cannot lock ref 'refs/heads/main': is at 1 but expected 2",
                PreconditionChanged,
            ),
            (
                "fatal: Authentication failed for 'https://example.com/r.git/'",
                Auth,
            ),
            (
                "fatal: could not read Username for 'https://example.com': terminal prompts disabled",
                Auth,
            ),
            ("git@example.com: Permission denied (publickey).", Auth),
            (
                "fatal: unable to access 'https://example.com/': Could not resolve host: example.com",
                Network,
            ),
            (
                "On branch main\nnothing to commit, working tree clean",
                Expected,
            ),
            ("Aborting commit due to empty commit message.", Expected),
            (
                " ! [rejected]        main -> main (non-fast-forward)",
                Expected,
            ),
            ("CONFLICT (content): Merge conflict in a.txt", Expected),
            (
                "fatal: destination path '/w/r' already exists and is not an empty directory.",
                Expected,
            ),
            ("fatal: repository '/nope' does not exist", Expected),
            (
                "error: the branch 'x' is not fully merged.\nIf you are sure you want to delete it, run 'git branch -D x'",
                Expected,
            ),
            (
                "error: Your local changes to the following files would be overwritten by checkout:\n\ta.txt",
                Expected,
            ),
            (
                "fatal: a branch named 'x' already exists",
                PreconditionChanged,
            ),
            (
                "error: cannot delete branch 'x' used by worktree at '/w'",
                Expected,
            ),
            (
                "CONFLICT (content): Merge conflict in a.txt\nThe stash entry is kept in case you need it again.",
                Expected,
            ),
            (
                "error: Your local changes to the following files would be overwritten by merge:\n\ta.txt",
                Expected,
            ),
            ("No stash entries found.", Expected),
            (" ! [rejected]        main -> main (stale info)", Expected),
            (
                " ! [remote rejected] main -> main (protected branch hook declined)",
                Expected,
            ),
            ("fatal: couldn't find remote ref nope", Expected),
            (
                "error: cannot pull with rebase: You have unstaged changes.",
                Expected,
            ),
            ("fatal: something nobody expected", Bug),
        ];
        for (output, expected) in cases {
            assert_eq!(class(output), Some(expected), "{output}");
        }
        assert_eq!(classify(true, "fatal: ignored"), OpOutcome::Succeeded);
    }

    #[test]
    fn saving_nothing_is_an_expected_failure() {
        let out = "No local changes to save\n";
        match classify_stash_save(true, out) {
            OpOutcome::Failed(failure) => assert_eq!(failure.class, ErrorClass::Expected),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            classify_stash_save(true, "Saved working directory and index state"),
            OpOutcome::Succeeded
        );
    }

    #[test]
    fn details_keep_the_raw_output() {
        let OpOutcome::Failed(failure) = classify(false, "fatal: x\n") else {
            panic!();
        };
        assert_eq!(failure.details, "fatal: x\n");
    }
}
