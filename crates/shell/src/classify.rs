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
            "non-fast-forward",
            "CONFLICT",
            "Automatic merge failed",
        ],
    ),
];

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
            ("fatal: something nobody expected", Bug),
        ];
        for (output, expected) in cases {
            assert_eq!(class(output), Some(expected), "{output}");
        }
        assert_eq!(classify(true, "fatal: ignored"), OpOutcome::Succeeded);
    }

    #[test]
    fn details_keep_the_raw_output() {
        let OpOutcome::Failed(failure) = classify(false, "fatal: x\n") else {
            panic!();
        };
        assert_eq!(failure.details, "fatal: x\n");
    }
}
