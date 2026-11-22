//! Task 14: commit, amend and merge conclusion end to end: the real core
//! driving the real shell on generated temp repos, with real git as the
//! oracle. System and global git config are shut out. Law 8 (amend and
//! commit undo exactly), law 9 (only the current branch moves) and law 12
//! (nothing runs while the lock is held) are checked at their edges here;
//! their owning tests stay in `undo.rs` and the model-based harness.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use invoke_core::{
    Action, Command as Cmd, CommitRefusal, ErrorClass, Event, Irreversible, MessageProblem, Notice,
    OpOutcome, Refusal, RepoId, RepoState, Snapshot, open, step,
};
use shell::classify::IDENTITY_UNKNOWN;
use shell::precondition::RepoPaths;
use shell::undo::Snapshotted;
use shell::{Git, GitRunner, UndoRunner, head_message, merge_message, read::read_event};

/// A temp repo, deleted on drop.
struct TempRepo {
    root: PathBuf,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
}

impl TempRepo {
    /// A repo whose committer is known from its own config (as a real user's
    /// is), not from the environment.
    fn new() -> Self {
        let repo = Self::bare();
        repo.git(&["config", "user.name", "Committer"]);
        repo.git(&["config", "user.email", "committer@example.com"]);
        repo
    }

    /// A repo that has no idea who is committing, and does not guess.
    fn bare() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let root = std::env::temp_dir().join(format!(
            "invoke-git-commit-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let workdir = root.join("repo");
        fs::create_dir_all(&workdir).unwrap();
        let global = root.join("gitconfig");
        fs::write(&global, "").unwrap();
        let env = [
            ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
            ("GIT_CONFIG_GLOBAL", global.into_os_string()),
        ]
        .into_iter()
        .map(|(key, value)| (OsString::from(key), value))
        .collect();
        let repo = Self {
            paths: RepoPaths {
                git_dir: workdir.join(".git"),
                workdir,
            },
            root,
            env,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.useConfigOnly", "true"]);
        repo
    }

    fn try_git(&self, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }

    fn git(&self, args: &[&str]) -> String {
        self.try_git(args)
            .unwrap_or_else(|| panic!("git {args:?} failed"))
    }

    fn write(&self, path: &str, text: &str) {
        fs::write(self.paths.workdir.join(path), text).unwrap();
    }

    fn stage(&self, path: &str, text: &str) {
        self.write(path, text);
        self.git(&["add", path]);
    }

    fn head(&self) -> Option<String> {
        self.try_git(&["rev-parse", "-q", "--verify", "HEAD"])
    }

    /// The raw bytes of the commit object at `HEAD`.
    fn commit_object(&self) -> Vec<u8> {
        let output = Command::new("git")
            .args(["cat-file", "commit", "HEAD"])
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout
    }

    /// Every ref but our own bookkeeping, as git lists them.
    fn refs(&self) -> BTreeSet<String> {
        self.git(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .lines()
            .filter(|line| !line.starts_with("refs/invoke-git/"))
            .map(str::to_owned)
            .collect()
    }

    fn saved_refs(&self) -> Vec<String> {
        self.git(&["for-each-ref", "--format=%(refname)", "refs/invoke-git/"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn hook(&self, name: &str, body: &str) {
        let hooks = self.paths.git_dir.join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join(name);
        fs::write(&hook, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn merge_head_exists(&self) -> bool {
        self.paths.git_dir.join("MERGE_HEAD").exists()
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The real core driving the real shell, one command at a time.
struct Driver<'a> {
    repo: &'a TempRepo,
    state: RepoState,
    runner: GitRunner,
    undo: UndoRunner,
}

impl<'a> Driver<'a> {
    fn open(repo: &'a TempRepo) -> Self {
        let git = || Git::detect("git").unwrap();
        let (state, commands) = open(RepoId {
            root_commit: None,
            remote_url: None,
        });
        let mut driver = Self {
            repo,
            state,
            runner: GitRunner::new(git(), repo.paths.clone()).with_env(repo.env.clone()),
            undo: UndoRunner::new(git(), repo.paths.clone()).with_env(repo.env.clone()),
        };
        for command in commands {
            driver.exec(command);
        }
        driver
    }

    fn deliver(&mut self, event: Event) {
        let (state, commands) = step(self.state.clone(), event);
        self.state = state;
        for command in commands {
            self.exec(command);
        }
    }

    fn exec(&mut self, command: Cmd) {
        match command {
            Cmd::ReadState { read, .. } => self.deliver(read_event(&self.repo.paths, read)),
            Cmd::SnapshotForUndo(point) => match self.undo.snapshot(&point) {
                Snapshotted::Taken(event) | Snapshotted::Failed(event) => self.deliver(event),
            },
            Cmd::RunGit { op, args, expect } => {
                let (sender, events) = mpsc::channel();
                self.runner.run(op, args, expect, sender);
                loop {
                    let event = events
                        .recv_timeout(Duration::from_secs(30))
                        .expect("no OpFinished in 30 s");
                    let done = matches!(event, Event::OpFinished(..));
                    self.deliver(event);
                    if done {
                        break;
                    }
                }
            }
            Cmd::ReplayUndo(replay) => self.deliver(self.undo.replay(&replay)),
            Cmd::PruneUndo(ops) => self.undo.prune(&ops),
            other => panic!("unexpected command {other:?}"),
        }
    }

    /// What the watcher would send: read everything again.
    fn refresh(&mut self) {
        self.deliver(Event::RefreshRequested);
    }

    fn commit(&mut self, message: &str, amend: bool) {
        self.deliver(Event::CommitRequested {
            message: message.into(),
            amend,
        });
    }

    fn outcome(&self) -> Option<&OpOutcome> {
        match &self.state.notice {
            Some(Notice::OpEnded { outcome, .. }) => Some(outcome),
            _ => None,
        }
    }

    fn refusal(&self) -> Option<&Refusal> {
        match &self.state.notice {
            Some(Notice::Refused {
                action: Action::Commit,
                reason,
            }) => Some(reason),
            _ => None,
        }
    }

    fn failure(&self) -> (ErrorClass, String) {
        match self.outcome() {
            Some(OpOutcome::Failed(failure)) => (failure.class, failure.details.clone()),
            other => panic!(
                "expected a failure, got {other:?} / {:?}",
                self.state.notice
            ),
        }
    }

    fn undo_label(&self) -> Option<String> {
        self.state
            .undo
            .entries
            .back()
            .map(|entry| entry.point.label.clone())
    }

    fn is_fresh(&self) -> bool {
        matches!(self.state.snapshot, Snapshot::Read { .. })
    }
}

fn seeded() -> TempRepo {
    let repo = TempRepo::new();
    repo.stage("a.txt", "one\n");
    repo.git(&["commit", "-q", "-m", "first"]);
    repo
}

#[test]
fn a_commit_with_a_subject_and_a_body() {
    let repo = seeded();
    repo.stage("a.txt", "two\n");
    let mut driver = Driver::open(&repo);
    assert!(driver.is_fresh());
    driver.commit("Fix login\n\nThe token was never refreshed.", false);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert_eq!(
        repo.git(&["log", "-1", "--format=%B"]),
        "Fix login\n\nThe token was never refreshed."
    );
    assert_eq!(driver.undo_label().as_deref(), Some("Commit 'Fix login'"));
    // Nothing is left staged, so the next plain commit is refused.
    driver.commit("Again", false);
    assert_eq!(
        driver.refusal(),
        Some(&Refusal::Commit(CommitRefusal::NothingStaged))
    );
}

/// Windows editors send CRLF. The commit holds LF only, never a `\r`, even
/// with `core.autocrlf` on.
#[test]
fn crlf_input_is_stored_as_lf() {
    let repo = seeded();
    repo.git(&["config", "core.autocrlf", "true"]);
    repo.stage("a.txt", "two\n");
    let mut driver = Driver::open(&repo);
    driver.commit("Subject  \r\n\r\n\r\nBody one\r\nBody two\r\n\r\n", false);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    let object = repo.commit_object();
    assert!(!object.contains(&b'\r'), "a CR reached the commit");
    let text = String::from_utf8(object).unwrap();
    assert!(
        text.ends_with("\n\nSubject\n\nBody one\nBody two\n"),
        "{text:?}"
    );
}

/// A message is committed as the editor shows it, whatever `commit.cleanup`
/// says: a line that starts with `#` is a line, not a comment.
#[test]
fn lines_starting_with_a_hash_survive() {
    let repo = seeded();
    repo.git(&["config", "commit.cleanup", "strip"]);
    repo.stage("a.txt", "two\n");
    let mut driver = Driver::open(&repo);
    driver.commit("#123 Fix login\n\n# not a comment\nbody", false);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert_eq!(
        repo.git(&["log", "-1", "--format=%B"]),
        "#123 Fix login\n\n# not a comment\nbody"
    );
}

#[test]
fn unicode_long_and_multiline_subjects_label_the_undo_point_correctly() {
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    let cases = [
        ("ünï 日本 😀", "Commit 'ünï 日本 😀'".to_owned()),
        (
            "\n\n  \nFirst real line\nsecond line",
            "Commit 'First real line'".to_owned(),
        ),
        (&"😀".repeat(100), format!("Commit '{}…'", "😀".repeat(60))),
    ];
    for (n, (message, label)) in cases.into_iter().enumerate() {
        repo.stage("a.txt", &format!("change {n}\n"));
        driver.refresh();
        driver.commit(message, false);
        assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded), "{message:?}");
        assert_eq!(driver.undo_label(), Some(label));
    }
    // The commit itself keeps the whole subject.
    assert_eq!(repo.git(&["log", "-1", "--format=%s"]), "😀".repeat(100));
}

#[test]
fn an_empty_commit_and_an_empty_message_are_refused_before_git_runs() {
    let repo = seeded();
    repo.write("a.txt", "unstaged only\n");
    let head = repo.head();
    let refs = repo.refs();
    let mut driver = Driver::open(&repo);

    driver.commit("Nothing staged", false);
    assert_eq!(
        driver.refusal(),
        Some(&Refusal::Commit(CommitRefusal::NothingStaged))
    );
    for (message, problem) in [
        ("", MessageProblem::Empty),
        ("  \r\n\t\n", MessageProblem::OnlyWhitespace),
    ] {
        repo.stage("a.txt", "staged now\n");
        driver.refresh();
        driver.commit(message, false);
        assert_eq!(
            driver.refusal(),
            Some(&Refusal::Commit(CommitRefusal::Message(problem)))
        );
        // Not even an amend takes an empty message.
        driver.commit(message, true);
        assert_eq!(
            driver.refusal(),
            Some(&Refusal::Commit(CommitRefusal::Message(problem)))
        );
    }
    assert_eq!(repo.head(), head);
    assert_eq!(repo.refs(), refs);
    assert!(driver.state.ops.running.is_none() && driver.state.ops.queued.is_empty());
    assert!(repo.saved_refs().is_empty(), "no undo point was taken");
}

/// If git itself finds nothing to commit (the core's snapshot was a moment
/// old), it is an expected outcome, not an error.
#[test]
fn git_refusing_an_empty_commit_is_an_expected_outcome() {
    let repo = seeded();
    let (sender, events) = mpsc::channel();
    let runner =
        GitRunner::new(Git::detect("git").unwrap(), repo.paths.clone()).with_env(repo.env.clone());
    let expect = shell::precondition::current(&repo.paths).unwrap();
    runner.run(
        invoke_core::OpId(1),
        invoke_core::GitArgs::Commit {
            message: "Empty\n".into(),
            amend: false,
        },
        expect,
        sender,
    );
    let outcome = loop {
        if let Event::OpFinished(_, outcome) = events.recv_timeout(Duration::from_secs(30)).unwrap()
        {
            break outcome;
        }
    };
    let OpOutcome::Failed(failure) = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(failure.class, ErrorClass::Expected);
}

/// The author stays; the committer and the message are new. The staged
/// change joins the commit. Law 9: no other ref moves.
#[test]
fn amend_keeps_the_author_and_takes_what_is_staged() {
    let repo = seeded();
    repo.git(&["branch", "dev"]);
    repo.stage("a.txt", "two\n");
    repo.git(&[
        "commit",
        "-q",
        "--author=Original Author <original@example.com>",
        "-m",
        "Second",
    ]);
    let parent = repo.git(&["rev-parse", "HEAD~1"]);
    let tip = repo.head().unwrap();
    repo.stage("b.txt", "extra\n");
    let mut driver = Driver::open(&repo);
    let refs_before = repo.refs();

    driver.commit("Second, reworded", true);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert_ne!(repo.head().unwrap(), tip);
    assert_eq!(repo.git(&["rev-parse", "HEAD~1"]), parent);
    assert_eq!(
        repo.git(&["log", "-1", "--format=%an <%ae>|%cn <%ce>|%s"]),
        "Original Author <original@example.com>|Committer <committer@example.com>|Second, reworded"
    );
    assert_eq!(
        repo.git(&["show", "--name-only", "--format=", "HEAD"]),
        "a.txt\nb.txt"
    );
    assert_eq!(
        driver.undo_label().as_deref(),
        Some("Amend 'Second, reworded'")
    );
    // Only `main` moved.
    let moved: Vec<_> = repo
        .refs()
        .symmetric_difference(&refs_before)
        .cloned()
        .collect();
    assert_eq!(moved.len(), 2, "{moved:?}");
    assert!(
        moved
            .iter()
            .all(|line| line.starts_with("refs/heads/main "))
    );

    // Law 8: undo puts the amended commit back exactly.
    driver.deliver(Event::UndoRequested);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert_eq!(repo.head().unwrap(), tip);
    assert_eq!(repo.refs(), refs_before);
    assert_eq!(repo.git(&["diff", "--cached", "--name-only"]), "b.txt");
}

/// A reword: nothing staged is fine for an amend.
#[test]
fn amend_with_nothing_staged_rewords() {
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    driver.commit("first, but better", true);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert_eq!(repo.git(&["log", "--format=%s"]), "first, but better");
    assert_eq!(repo.git(&["rev-list", "--count", "HEAD"]), "1");
}

#[test]
fn amending_an_unborn_branch_is_refused_and_the_first_commit_works() {
    let repo = TempRepo::new();
    repo.stage("a.txt", "one\n");
    let mut driver = Driver::open(&repo);
    assert_eq!(head_message(&repo.paths).unwrap(), None);

    driver.commit("Nope", true);
    assert_eq!(
        driver.refusal(),
        Some(&Refusal::Commit(CommitRefusal::AmendUnborn))
    );
    assert_eq!(repo.head(), None);

    driver.commit("First commit", false);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert_eq!(repo.git(&["log", "-1", "--format=%s"]), "First commit");
    assert_eq!(
        head_message(&repo.paths).unwrap().as_deref(),
        Some("First commit\n")
    );

    // Law 8 for the first commit: the branch is unborn again.
    driver.deliver(Event::UndoRequested);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert_eq!(repo.head(), None);
}

#[test]
fn the_previous_message_is_read_cleaned_for_the_amend_editor() {
    let repo = seeded();
    repo.stage("a.txt", "two\n");
    // Stored the way another client might: CRLF and trailing blanks.
    repo.git(&[
        "commit",
        "-q",
        "--cleanup=verbatim",
        "-m",
        "Subject  \r\n\r\n\r\nBody \r\n\r\n",
    ]);
    assert_eq!(
        head_message(&repo.paths).unwrap().as_deref(),
        Some("Subject\n\nBody\n")
    );
}

#[test]
fn a_rejecting_pre_commit_hook_is_reported_with_its_output() {
    let repo = seeded();
    repo.hook("pre-commit", "echo 'lint failed: a.txt' >&2\nexit 1");
    repo.stage("a.txt", "two\n");
    let head = repo.head();
    let refs = repo.refs();
    let mut driver = Driver::open(&repo);

    driver.commit("Blocked", false);
    let (class, details) = driver.failure();
    assert_eq!(class, ErrorClass::Rejected);
    assert!(
        details.starts_with("The pre-commit hook rejected the commit (exit code 1)."),
        "{details}"
    );
    assert!(details.contains("lint failed: a.txt"), "{details}");
    // Nothing moved, no undo point is left, and the saved tree is pruned.
    assert_eq!(repo.head(), head);
    assert_eq!(repo.refs(), refs);
    assert!(driver.state.undo.entries.is_empty());
    assert!(repo.saved_refs().is_empty());
    // The change is still staged: the user can fix and retry.
    assert_eq!(repo.git(&["diff", "--cached", "--name-only"]), "a.txt");
}

#[test]
fn a_hook_that_talks_like_git_is_still_a_rejection() {
    let repo = seeded();
    repo.hook(
        "commit-msg",
        "echo 'nothing to commit, subject too vague'\nexit 7",
    );
    repo.stage("a.txt", "two\n");
    let mut driver = Driver::open(&repo);
    driver.commit("x", false);
    let (class, details) = driver.failure();
    assert_eq!(class, ErrorClass::Rejected);
    assert!(
        details.starts_with("The commit-msg hook rejected the commit (exit code 7)."),
        "{details}"
    );
    assert!(details.contains("subject too vague"));
}

/// git ignores a failing `post-commit`: the commit happened.
#[test]
fn a_failing_post_commit_hook_does_not_fail_the_commit() {
    let repo = seeded();
    repo.hook("post-commit", "exit 1");
    repo.stage("a.txt", "two\n");
    let mut driver = Driver::open(&repo);
    driver.commit("Done", false);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert_eq!(repo.git(&["log", "-1", "--format=%s"]), "Done");
}

#[test]
fn a_missing_identity_is_an_environment_failure_with_a_clear_message() {
    let repo = TempRepo::bare();
    repo.stage("a.txt", "one\n");
    let mut driver = Driver::open(&repo);
    driver.commit("First", false);
    let (class, details) = driver.failure();
    assert_eq!(class, ErrorClass::Environment);
    assert!(details.starts_with(IDENTITY_UNKNOWN), "{details}");
    assert_eq!(repo.head(), None);
    assert!(driver.state.undo.entries.is_empty());
}

/// Two branches that conflict on `a.txt`, merged and left in progress.
fn merging_with_a_conflict(repo: &TempRepo) {
    repo.stage("a.txt", "base\n");
    repo.git(&["commit", "-q", "-m", "base"]);
    repo.git(&["switch", "-q", "-c", "topic"]);
    repo.stage("a.txt", "topic\n");
    repo.git(&["commit", "-q", "-m", "topic change"]);
    repo.git(&["switch", "-q", "main"]);
    repo.stage("a.txt", "main\n");
    repo.git(&["commit", "-q", "-m", "main change"]);
    assert!(repo.try_git(&["merge", "--no-ff", "topic"]).is_none());
    assert!(repo.merge_head_exists());
}

#[test]
fn a_conflicted_merge_cannot_be_committed_until_it_is_resolved_then_the_commit_concludes_it() {
    let repo = TempRepo::new();
    merging_with_a_conflict(&repo);
    let head = repo.head();
    let mut driver = Driver::open(&repo);

    driver.commit("Merge topic", false);
    assert_eq!(
        driver.refusal(),
        Some(&Refusal::Commit(CommitRefusal::UnresolvedConflicts))
    );
    assert_eq!(repo.head(), head);

    // Resolve it; the editor starts from git's message without its notes.
    repo.stage("a.txt", "both\n");
    driver.refresh();
    assert_eq!(
        merge_message(&repo.paths).unwrap().as_deref(),
        Some("Merge branch 'topic'\n")
    );
    // An amend would rewrite the commit under the merge: refused.
    driver.commit("Merge topic", true);
    assert_eq!(
        driver.refusal(),
        Some(&Refusal::Commit(CommitRefusal::AmendDuringMerge))
    );

    driver.commit("Merge branch 'topic'", false);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert_eq!(
        driver.undo_label().as_deref(),
        Some("Conclude merge 'Merge branch 'topic''")
    );
    assert!(!repo.merge_head_exists());
    assert_eq!(
        repo.git(&["rev-list", "--parents", "-1", "HEAD"])
            .split(' ')
            .count(),
        3
    );
    assert_eq!(repo.git(&["status", "--porcelain"]), "");

    // Undoing it would drop the merge's second parent: the undo says so
    // and changes nothing.
    let after = (repo.head(), repo.refs());
    driver.deliver(Event::UndoRequested);
    assert_eq!(
        driver.state.notice,
        Some(Notice::Refused {
            action: Action::Undo,
            reason: Refusal::NotUndoable(Irreversible::MergeConcluded)
        })
    );
    assert_eq!((repo.head(), repo.refs()), after);
}

/// A merge may leave the tree as it was (`-s ours`): nothing is staged, and
/// the commit is still the right way to conclude it.
#[test]
fn a_merge_with_nothing_staged_is_concluded_by_a_commit() {
    let repo = seeded();
    repo.git(&["switch", "-q", "-c", "topic"]);
    repo.stage("b.txt", "topic\n");
    repo.git(&["commit", "-q", "-m", "topic work"]);
    repo.git(&["switch", "-q", "main"]);
    repo.stage("a.txt", "main\n");
    repo.git(&["commit", "-q", "-m", "main work"]);
    repo.git(&[
        "merge",
        "-q",
        "--no-commit",
        "--no-ff",
        "-s",
        "ours",
        "topic",
    ]);
    assert!(repo.merge_head_exists());
    assert_eq!(repo.git(&["diff", "--cached", "--name-only"]), "");
    let mut driver = Driver::open(&repo);

    driver.commit("Merge branch 'topic' (ours)", false);
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert!(!repo.merge_head_exists());
    assert_eq!(
        repo.git(&["rev-list", "--parents", "-1", "HEAD"])
            .split(' ')
            .count(),
        3
    );
}

/// Law 12: a commit waits while someone else holds `index.lock`, and runs
/// when the lock is gone.
#[test]
fn a_commit_waits_for_the_lock() {
    let repo = seeded();
    repo.stage("a.txt", "two\n");
    let lock = repo.paths.git_dir.join("index.lock");
    fs::write(&lock, b"").unwrap();
    let head = repo.head();
    let mut driver = Driver::open(&repo);
    driver.commit("Later", false);
    assert_eq!(repo.head(), head);
    assert_eq!(driver.state.ops.queued.len(), 1);

    fs::remove_file(&lock).unwrap();
    driver.refresh();
    assert_eq!(driver.outcome(), Some(&OpOutcome::Succeeded));
    assert_ne!(repo.head(), head);
}
