//! The runner against real git in generated temp repos. System and global
//! git config are shut out, so the user's setup never leaks in.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use invoke_core::{ErrorClass, Event, Expect, GitArgs, Head, IndexId, OpId, OpOutcome};
use shell::precondition::{self, RepoPaths};
use shell::{Git, GitRunner};

/// A temp repo, deleted on drop.
struct TempRepo {
    root: PathBuf,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
}

impl TempRepo {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let root = std::env::temp_dir().join(format!(
            "invoke-git-shell-{}-{}-{nanos}",
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
            ("GIT_AUTHOR_NAME", "Test".into()),
            ("GIT_AUTHOR_EMAIL", "test@example.com".into()),
            ("GIT_COMMITTER_NAME", "Test".into()),
            ("GIT_COMMITTER_EMAIL", "test@example.com".into()),
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
        repo
    }

    /// Plain git for setting up fixtures and checking results.
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write(&self, path: &str, text: &str) {
        fs::write(self.paths.workdir.join(path), text).unwrap();
    }

    fn head(&self) -> Option<String> {
        let output = Command::new("git")
            .args(["rev-parse", "-q", "--verify", "HEAD"])
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }

    fn runner(&self) -> GitRunner {
        GitRunner::new(Git::detect("git").unwrap(), self.paths.clone()).with_env(self.env.clone())
    }

    fn expect(&self) -> Expect {
        precondition::current(&self.paths).unwrap()
    }

    /// A pre-commit hook that marks when it starts, sleeps, then marks when
    /// it would have finished.
    fn slow_hook(&self) {
        let hooks = self.paths.git_dir.join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        fs::write(
            &hook,
            "#!/bin/sh\ntouch hook-started\nsleep 3\ntouch hook-finished\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn exists(&self, path: &str) -> bool {
        self.paths.workdir.join(path).exists()
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn commit(message: &str) -> GitArgs {
    GitArgs::Commit {
        message: message.into(),
        amend: false,
    }
}

/// Collects events for `op` until its `OpFinished`.
fn finish(events: &Receiver<Event>, op: OpId) -> (Vec<Event>, OpOutcome) {
    let mut seen = Vec::new();
    loop {
        match events
            .recv_timeout(Duration::from_secs(30))
            .expect("no OpFinished in 30 s")
        {
            Event::OpFinished(id, outcome) if id == op => return (seen, outcome),
            event => seen.push(event),
        }
    }
}

fn failure_class(outcome: &OpOutcome) -> Option<ErrorClass> {
    match outcome {
        OpOutcome::Failed(failure) => Some(failure.class),
        _ => None,
    }
}

#[test]
fn system_git_is_new_enough() {
    assert!(Git::detect("git").is_ok());
    assert!(matches!(
        Git::detect("definitely-not-git-xyz"),
        Err(shell::SetupError::Missing(_))
    ));
}

#[test]
fn commit_runs_and_moves_head() {
    let repo = TempRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "a.txt"]);
    let expect = repo.expect();
    assert!(matches!(expect.head, Head::Unborn(_)));
    assert!(matches!(expect.index, IndexId::Checksum(_)));

    let (sender, events) = mpsc::channel();
    repo.runner()
        .run(OpId(1), commit("First\n\nBody line"), expect, sender);
    let (_, outcome) = finish(&events, OpId(1));
    assert_eq!(outcome, OpOutcome::Succeeded);
    assert_eq!(
        repo.git(&["log", "-1", "--format=%B"]),
        "First\n\nBody line"
    );
    assert!(matches!(repo.expect().head, Head::Branch { .. }));
}

#[test]
fn stale_head_is_refused_and_nothing_changes() {
    let repo = TempRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "a.txt"]);
    repo.git(&["commit", "-q", "-m", "first"]);
    repo.write("a.txt", "two\n");
    repo.git(&["add", "a.txt"]);
    let stale = repo.expect();
    // Someone else moves the branch behind our back, leaving the index as
    // it was, so only the head check can catch it.
    let outside = repo.git(&["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "outside"]);
    repo.git(&["update-ref", "refs/heads/main", &outside]);
    assert_eq!(repo.expect().index, stale.index);

    let (sender, events) = mpsc::channel();
    repo.runner().run(OpId(1), commit("ours"), stale, sender);
    let (_, outcome) = finish(&events, OpId(1));
    assert_eq!(
        failure_class(&outcome),
        Some(ErrorClass::PreconditionChanged),
        "{outcome:?}"
    );
    assert_eq!(repo.head(), Some(outside));
}

#[test]
fn stale_index_is_refused_and_nothing_changes() {
    let repo = TempRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "a.txt"]);
    let stale = repo.expect();
    repo.write("b.txt", "two\n");
    repo.git(&["add", "b.txt"]);

    let (sender, events) = mpsc::channel();
    repo.runner().run(OpId(1), commit("ours"), stale, sender);
    let (_, outcome) = finish(&events, OpId(1));
    assert_eq!(
        failure_class(&outcome),
        Some(ErrorClass::PreconditionChanged),
        "{outcome:?}"
    );
    assert_eq!(repo.head(), None);
}

#[test]
fn held_index_lock_is_refused_and_left_alone() {
    let repo = TempRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "a.txt"]);
    let expect = repo.expect();
    let lock = repo.paths.git_dir.join("index.lock");
    fs::write(&lock, "held by someone else").unwrap();

    let (sender, events) = mpsc::channel();
    repo.runner().run(OpId(1), commit("ours"), expect, sender);
    let (_, outcome) = finish(&events, OpId(1));
    assert_eq!(
        failure_class(&outcome),
        Some(ErrorClass::PreconditionChanged),
        "{outcome:?}"
    );
    // Refused by the shell's own check, before git was started.
    let OpOutcome::Failed(failure) = &outcome else {
        unreachable!()
    };
    assert!(
        failure.details.contains("another git process is running"),
        "{failure:?}"
    );
    assert_eq!(fs::read_to_string(&lock).unwrap(), "held by someone else");
    assert_eq!(repo.head(), None);
}

#[test]
fn nothing_to_commit_is_an_expected_outcome() {
    let repo = TempRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "a.txt"]);
    repo.git(&["commit", "-q", "-m", "first"]);

    let (sender, events) = mpsc::channel();
    repo.runner()
        .run(OpId(1), commit("again"), repo.expect(), sender);
    let (_, outcome) = finish(&events, OpId(1));
    assert_eq!(
        failure_class(&outcome),
        Some(ErrorClass::Expected),
        "{outcome:?}"
    );
}

#[test]
fn cancel_kills_the_hook_and_leaves_head_alone() {
    let repo = TempRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "a.txt"]);
    repo.slow_hook();

    let runner = repo.runner();
    let (sender, events) = mpsc::channel();
    runner.run(OpId(7), commit("slow"), repo.expect(), sender);
    let started = Instant::now();
    while !repo.exists("hook-started") {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "hook never started"
        );
        thread::sleep(Duration::from_millis(20));
    }
    runner.cancel(OpId(7));
    let cancelled_at = Instant::now();
    let (_, outcome) = finish(&events, OpId(7));
    assert_eq!(outcome, OpOutcome::Cancelled);
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(3),
        "cancel took {:?}",
        cancelled_at.elapsed()
    );

    // The hook's `sleep` is a grandchild of git; had it survived, it would
    // touch the marker after 3 s.
    thread::sleep(Duration::from_secs(4));
    assert!(
        !repo.exists("hook-finished"),
        "the hook outlived the cancel"
    );
    assert_eq!(repo.head(), None);
    assert!(!repo.paths.git_dir.join("index.lock").exists());
}

#[test]
fn cancel_right_after_start_is_cancelled() {
    let repo = TempRepo::new();
    repo.write("a.txt", "one\n");
    repo.git(&["add", "a.txt"]);
    repo.slow_hook();
    let runner = repo.runner();
    let (sender, events) = mpsc::channel();
    runner.run(OpId(1), commit("x"), repo.expect(), sender);
    runner.cancel(OpId(1));
    let (_, outcome) = finish(&events, OpId(1));
    assert_eq!(outcome, OpOutcome::Cancelled);
    assert_eq!(repo.head(), None);
}

#[test]
fn unknown_ops_ignore_cancel() {
    let repo = TempRepo::new();
    repo.runner().cancel(OpId(99));
}
