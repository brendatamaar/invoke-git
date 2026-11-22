//! Turning a finished git process into the core's error classes, once, here
//! (`docs/spec/core-design.md` → "Errors"). git's messages are matched in
//! English, which `LANGUAGE=en` guarantees.

use invoke_core::{ErrorClass, Failure, OpOutcome};

/// What git says when it has no name or email to commit with.
const IDENTITY_MARKERS: [&str; 6] = [
    "Author identity unknown",
    "Committer identity unknown",
    "Please tell me who you are",
    "unable to auto-detect email address",
    // `user.useConfigOnly`: nothing is guessed from the host name.
    "auto-detection is disabled",
    "empty ident name",
];

/// Markers checked in order; the first class with a match wins. Auth comes
/// before network because `unable to access` also wraps auth failures.
const MARKERS: [(ErrorClass, &[&str]); 5] = [
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
    // Task 14: the setup is wrong, not the request. Before `Expected`
    // because these come next to `fatal:` lines of their own.
    (ErrorClass::Environment, &IDENTITY_MARKERS),
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
            // Task 23: the conflict flow. A continue that runs into the next
            // conflict, one that git turns down, or an abort or skip with
            // nothing left to do; the snapshot says where things stand.
            "Could not apply",
            "unmerged files",
            "You must edit all merge conflicts",
            "No changes - did you forget to use 'git add'",
            "There is no merge to abort",
            "No rebase in progress",
            "no cherry-pick or revert in progress",
            "There is no merge in progress",
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

/// Task 14. The text the user sees when git has no identity to commit with
/// (it leads the details, ahead of git's own words).
pub const IDENTITY_UNKNOWN: &str =
    "Git does not know who you are. Set user.name and user.email, then commit again.";

/// Whether git refused because no name or email is configured.
pub fn is_identity_missing(output: &str) -> bool {
    IDENTITY_MARKERS
        .iter()
        .any(|marker| output.contains(marker))
}

/// A commit hook that ended the run by exiting non-zero (task 14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookFailure {
    pub hook: String,
    pub code: i64,
}

/// The hooks that can stop a commit. A failing `post-commit` is ignored by
/// git, so it never ends a run.
const BLOCKING_HOOKS: [&str; 4] = [
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "pre-merge-commit",
];

/// Reads git's trace2 event stream (`GIT_TRACE2_EVENT`, one JSON object
/// per line) for a blocking hook that exited non-zero. git prints nothing
/// of its own when a hook fails, and the hook may print anything (even
/// `nothing to commit`), so the output cannot say; the trace can.
///
/// A hook is matched to its exit by process (`sid`) and `child_id`: hooks
/// that run their own git write to the same file.
pub fn failed_hook(trace: &str) -> Option<HookFailure> {
    let mut started: Vec<(&str, &str, &str)> = Vec::new();
    for line in trace.lines() {
        let (Some(event), Some(sid), Some(child)) = (
            text_field(line, "event"),
            text_field(line, "sid"),
            number_field(line, "child_id"),
        ) else {
            continue;
        };
        match event {
            "child_start" if text_field(line, "child_class") == Some("hook") => {
                if let Some(hook) = text_field(line, "hook_name") {
                    started.push((sid, child, hook));
                }
            }
            "child_exit" => {
                let hook = started
                    .iter()
                    .find(|(s, c, _)| *s == sid && *c == child)
                    .map(|(_, _, hook)| *hook);
                let code = number_field(line, "code").and_then(|code| code.parse::<i64>().ok());
                if let (Some(hook), Some(code)) = (hook, code)
                    && code != 0
                    && BLOCKING_HOOKS.contains(&hook)
                {
                    return Some(HookFailure {
                        hook: hook.to_owned(),
                        code,
                    });
                }
            }
            _ => {}
        }
    }
    None
}

/// `"key":"value"` in one trace line. The values read here (event names,
/// session ids, hook names) never contain a quote.
fn text_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\":\"");
    let rest = &line[line.find(&pattern)? + pattern.len()..];
    Some(&rest[..rest.find('"')?])
}

/// `"key":123` in one trace line, as its digits.
fn number_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("\"{key}\":");
    let rest = &line[line.find(&pattern)? + pattern.len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// A finished `git commit`, classified with its trace2 stream.
///
/// A blocking hook that failed makes it [`ErrorClass::Rejected`], whatever
/// the hook printed. A missing identity is an `Environment` failure with a
/// clear first line. Everything else is [`classify`]'s call.
pub fn classify_commit(success: bool, output: &str, trace: &str) -> OpOutcome {
    if success {
        return OpOutcome::Succeeded;
    }
    if let Some(HookFailure { hook, code }) = failed_hook(trace) {
        return OpOutcome::Failed(Failure {
            class: ErrorClass::Rejected,
            details: format!("The {hook} hook rejected the commit (exit code {code}).\n{output}"),
        });
    }
    match classify(false, output) {
        OpOutcome::Failed(failure)
            if failure.class == ErrorClass::Environment && is_identity_missing(output) =>
        {
            OpOutcome::Failed(Failure {
                details: format!("{IDENTITY_UNKNOWN}\n\n{output}"),
                ..failure
            })
        }
        other => other,
    }
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
            // Task 23: the conflict flow.
            (
                "CONFLICT (content): Merge conflict in a.txt\nerror: could not apply 1a2b3c4... t2\nCould not apply 1a2b3c4... t2",
                Expected,
            ),
            (
                "error: Committing is not possible because you have unmerged files.",
                Expected,
            ),
            (
                "fatal: There is no merge to abort (MERGE_HEAD missing).",
                Expected,
            ),
            ("fatal: No rebase in progress?", Expected),
            ("error: no cherry-pick or revert in progress", Expected),
            (
                "No changes - did you forget to use 'git add'?\nIf there is nothing left to stage, chances are that something else\nalready introduced the same changes; you might want to skip this patch.",
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

    // Task 14: commit hooks and the missing identity.

    /// The trace2 lines git 2.43 wrote for a failing hook (shortened).
    fn hook_trace(hook: &str, code: i64) -> String {
        format!(
            "{{\"event\":\"version\",\"sid\":\"S1\",\"exe\":\"2.43.0\"}}\n\
             {{\"event\":\"child_start\",\"sid\":\"S1\",\"child_id\":0,\"child_class\":\"hook\",\"hook_name\":\"{hook}\",\"use_shell\":false,\"argv\":[\".git/hooks/{hook}\"]}}\n\
             {{\"event\":\"child_exit\",\"sid\":\"S1\",\"child_id\":0,\"pid\":7,\"code\":{code},\"t_rel\":0.001}}\n"
        )
    }

    fn commit_failure(output: &str, trace: &str) -> Failure {
        match classify_commit(false, output, trace) {
            OpOutcome::Failed(failure) => failure,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_failed_blocking_hook_is_rejected_whatever_it_printed() {
        for hook in BLOCKING_HOOKS {
            // The hook's own words look like an expected outcome and an
            // unexpected one; the trace decides.
            for output in [
                "nope\n",
                "nothing to commit\n",
                "CONFLICT in your lint config\n",
                "",
            ] {
                let failure = commit_failure(output, &hook_trace(hook, 3));
                assert_eq!(failure.class, ErrorClass::Rejected, "{hook}: {output}");
                assert!(failure.details.starts_with(&format!(
                    "The {hook} hook rejected the commit (exit code 3)."
                )));
                assert!(failure.details.ends_with(output));
            }
        }
    }

    #[test]
    fn only_a_failing_blocking_hook_counts() {
        // `post-commit` is ignored by git; a hook that passed does not count.
        assert_eq!(failed_hook(&hook_trace("post-commit", 1)), None);
        assert_eq!(failed_hook(&hook_trace("pre-commit", 0)), None);
        assert_eq!(failed_hook(""), None);
        // So the output is classified as before.
        let failure = commit_failure("nothing to commit\n", &hook_trace("pre-commit", 0));
        assert_eq!(failure.class, ErrorClass::Expected);
        assert_eq!(
            classify_commit(true, "", &hook_trace("pre-commit", 1)),
            OpOutcome::Succeeded
        );
    }

    #[test]
    fn a_hooks_own_git_is_not_mistaken_for_the_hook() {
        // Same child id in another process (the hook ran `git status`): its
        // non-zero exit is not the hook's.
        let trace = "\
{\"event\":\"child_start\",\"sid\":\"S1\",\"child_id\":0,\"child_class\":\"hook\",\"hook_name\":\"pre-commit\"}\n\
{\"event\":\"child_exit\",\"sid\":\"S2\",\"child_id\":0,\"code\":128}\n\
{\"event\":\"child_exit\",\"sid\":\"S1\",\"child_id\":0,\"code\":0}\n";
        assert_eq!(failed_hook(trace), None);
        let trace = trace.replace("\"code\":0", "\"code\":9");
        assert_eq!(
            failed_hook(&trace),
            Some(HookFailure {
                hook: "pre-commit".into(),
                code: 9
            })
        );
    }

    #[test]
    fn a_missing_identity_is_an_environment_failure_with_a_clear_first_line() {
        for output in [
            "Author identity unknown\n\n*** Please tell me who you are.\n\nRun\n\n  git config --global user.email \"you@example.com\"\n\nfatal: unable to auto-detect email address (got 'u@h.(none)')\n",
            "fatal: empty ident name (for <>) not allowed\n",
            "Committer identity unknown\n",
        ] {
            let failure = commit_failure(output, "");
            assert_eq!(failure.class, ErrorClass::Environment, "{output}");
            assert!(failure.details.starts_with(IDENTITY_UNKNOWN));
            assert!(failure.details.ends_with(output));
            assert!(is_identity_missing(output));
        }
        // Other operations get the class too, with the raw output.
        assert_eq!(
            class("*** Please tell me who you are."),
            Some(ErrorClass::Environment)
        );
    }

    proptest::proptest! {
        /// Whatever a trace holds, reading it never panics, and a hook
        /// failure it finds is one of the blocking hooks.
        #[test]
        fn reading_a_trace_never_panics(trace in "\\PC{0,200}") {
            if let Some(found) = failed_hook(&trace) {
                proptest::prop_assert!(BLOCKING_HOOKS.contains(&found.hook.as_str()));
            }
        }
    }

    #[test]
    fn details_keep_the_raw_output() {
        let OpOutcome::Failed(failure) = classify(false, "fatal: x\n") else {
            panic!();
        };
        assert_eq!(failure.details, "fatal: x\n");
    }
}
