//! Clones through `CloneRunner` against local bare repos (file:// URLs), in
//! temp folders with system and global git config shut out. No network.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use invoke_core::clone::{CloneEvent, CloneRequest};
use invoke_core::{ErrorClass, OpId, OpOutcome};
use shell::{CloneRunner, Git};

struct Fixture {
    root: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl Fixture {
    /// `global_config` is the text of the isolated global git config.
    fn new(global_config: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let root = std::env::temp_dir().join(format!(
            "invoke-git-clone-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let global = root.join("gitconfig");
        fs::write(&global, global_config).unwrap();
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
        Self { root, env }
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
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

    /// A bare repo with a few commits; returns its file:// URL.
    fn source(&self) -> String {
        let work = self.root.join("src");
        fs::create_dir_all(&work).unwrap();
        self.git(&work, &["init", "-q", "-b", "main"]);
        for n in 0..5 {
            fs::write(
                work.join(format!("f{n}.txt")),
                format!("content {n}\n").repeat(200),
            )
            .unwrap();
            self.git(&work, &["add", "."]);
            self.git(&work, &["commit", "-q", "-m", &format!("commit {n}")]);
        }
        let bare = self.root.join("origin.git");
        self.git(
            &self.root,
            &["clone", "-q", "--bare", "src", bare.to_str().unwrap()],
        );
        file_url(&bare)
    }

    fn runner(&self) -> CloneRunner {
        // These tests never prompt, so the helper is never started.
        CloneRunner::new(Git::detect("git").unwrap(), self.root.join("no-askpass"))
            .with_env(self.env.clone())
    }

    fn start(
        &self,
        runner: &CloneRunner,
        url: &str,
        target: &Path,
    ) -> (OpId, Receiver<CloneEvent>) {
        let (events, receiver) = mpsc::channel();
        let op = OpId(1);
        runner.run(
            op,
            CloneRequest {
                url: url.to_owned(),
                target: target.to_owned(),
            },
            events,
        );
        (op, receiver)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// Collects events until `Finished`, which must be last and unique.
fn finish(op: OpId, events: &Receiver<CloneEvent>) -> (Vec<CloneEvent>, OpOutcome) {
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("clone did not finish");
        if let CloneEvent::Finished(finished, outcome) = event {
            assert_eq!(finished, op);
            // Nothing may follow the end.
            assert!(
                events.recv_timeout(Duration::from_millis(300)).is_err(),
                "events after Finished"
            );
            return (seen, outcome);
        }
        seen.push(event);
    }
}

#[test]
fn clone_succeeds_and_reports_progress() {
    let fx = Fixture::new("");
    let url = fx.source();
    let runner = fx.runner();
    // The leading folders do not exist yet; git creates them.
    let target = fx.root.join("work").join("repo");
    let (op, events) = fx.start(&runner, &url, &target);
    let (seen, outcome) = finish(op, &events);
    assert_eq!(outcome, OpOutcome::Succeeded);
    assert!(
        seen.iter()
            .any(|event| matches!(event, CloneEvent::Progress(id, _) if *id == op)),
        "no progress events: {seen:?}"
    );
    assert!(target.join(".git").is_dir());
    assert!(target.join("f4.txt").is_file());
    assert_eq!(fx.git(&target, &["log", "--format=%s", "-1"]), "commit 4");
}

#[test]
fn cancel_mid_clone_leaves_no_folder() {
    // A hook that makes the server side of the clone slow; its grandchild
    // `sleep` must die with the clone too.
    let slow = {
        let dir = std::env::temp_dir().join(format!("invoke-git-slow-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    };
    let hook = slow.join("hook.sh");
    let started = slow.join("started");
    let finished = slow.join("finished");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\ntouch '{}'\nsleep 3\ntouch '{}'\nexec \"$@\"\n",
            started.display(),
            finished.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let hook_path = hook.to_string_lossy().replace('\\', "/");
    let fx = Fixture::new(&format!("[uploadpack]\n\tpackObjectsHook = {hook_path}\n"));
    let url = fx.source();
    let runner = fx.runner();
    let work = fx.root.join("work");
    let target = work.join("deep").join("repo");
    let (op, events) = fx.start(&runner, &url, &target);

    let deadline = Instant::now() + Duration::from_secs(20);
    while !started.exists() {
        assert!(Instant::now() < deadline, "the slow hook never started");
        thread::sleep(Duration::from_millis(20));
    }
    let cancelled_at = Instant::now();
    runner.cancel(op);
    let (_, outcome) = finish(op, &events);
    assert_eq!(outcome, OpOutcome::Cancelled);
    assert!(cancelled_at.elapsed() < Duration::from_millis(2900));
    // Nothing the clone created is left, including the folders above it.
    assert!(!target.exists());
    assert!(!work.exists());
    assert!(fx.root.exists());
    // The hook's `sleep` was killed with the tree: it never gets to finish.
    thread::sleep(Duration::from_millis(3500));
    assert!(!finished.exists(), "the hook outlived the cancel");
    let _ = fs::remove_dir_all(&slow);
}

#[test]
fn a_failed_clone_leaves_an_existing_empty_folder_empty() {
    let fx = Fixture::new("");
    let runner = fx.runner();
    let target = fx.root.join("empty");
    fs::create_dir(&target).unwrap();
    let (op, events) = fx.start(&runner, &file_url(&fx.root.join("missing.git")), &target);
    let (_, outcome) = finish(op, &events);
    let OpOutcome::Failed(failure) = outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert_eq!(failure.class, ErrorClass::Expected, "{}", failure.details);
    assert!(target.is_dir());
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
}

#[test]
fn a_non_empty_target_is_refused_and_untouched() {
    let fx = Fixture::new("");
    let url = fx.source();
    let runner = fx.runner();
    let target = fx.root.join("busy");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("mine.txt"), "keep me").unwrap();
    let (op, events) = fx.start(&runner, &url, &target);
    let (_, outcome) = finish(op, &events);
    assert!(matches!(
        outcome,
        OpOutcome::Failed(ref failure) if failure.class == ErrorClass::Expected
    ));
    assert_eq!(
        fs::read_to_string(target.join("mine.txt")).unwrap(),
        "keep me"
    );
}

#[test]
fn a_relative_target_is_refused() {
    let fx = Fixture::new("");
    let runner = fx.runner();
    let (op, events) = fx.start(&runner, "file:///nowhere", Path::new("relative/repo"));
    let (_, outcome) = finish(op, &events);
    assert!(matches!(outcome, OpOutcome::Failed(_)));
    assert!(!Path::new("relative").exists());
}

#[test]
fn cancel_before_the_clone_starts_creates_nothing() {
    let fx = Fixture::new("");
    let url = fx.source();
    let runner = fx.runner();
    let target = fx.root.join("never");
    let (op, events) = fx.start(&runner, &url, &target);
    runner.cancel(op);
    let (_, outcome) = finish(op, &events);
    // Either the cancel won, or the clone was fast enough to finish first;
    // both leave a consistent folder.
    match outcome {
        OpOutcome::Cancelled => assert!(!target.exists()),
        OpOutcome::Succeeded => assert!(target.join(".git").is_dir()),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn cancel_of_an_unknown_job_is_a_no_op() {
    let fx = Fixture::new("");
    fx.runner().cancel(OpId(99));
}
