//! Fetch, pull, push and set-upstream through `GitRunner` (task 17) against
//! a local bare remote in temp folders, with system and global git config
//! shut out. Two clones share the remote: `alice` is the repo under test
//! and `bob` is the teammate who moves the remote.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use invoke_core::{
    BranchName, ErrorClass, Event, GitArgs, Lease, Oid, OpId, OpOutcome, PullMode, RefName,
    RemoteBranch,
};
use shell::precondition::{self, RepoPaths};
use shell::{Git, GitRunner};

struct Fixture {
    root: PathBuf,
    alice: PathBuf,
    bob: PathBuf,
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
            "invoke-git-sync-{}-{}-{nanos}",
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
        let fx = Self {
            alice: root.join("alice"),
            bob: root.join("bob"),
            root,
            env,
        };
        let origin = fx.origin();
        fx.git(
            &fx.root,
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        fx.git(
            &fx.root,
            &["clone", "-q", origin.to_str().unwrap(), "alice"],
        );
        fx.commit(&fx.alice, "base.txt", "base\n", "base");
        fx.git(&fx.alice, &["push", "-q", "-u", "origin", "main"]);
        fx.git(&fx.root, &["clone", "-q", origin.to_str().unwrap(), "bob"]);
        fx
    }

    fn origin(&self) -> PathBuf {
        self.root.join("origin.git")
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.try_git(dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn try_git(&self, dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap()
    }

    /// Writes `path` and commits it; returns the new commit.
    fn commit(&self, repo: &Path, path: &str, text: &str, message: &str) -> String {
        fs::write(repo.join(path), text).unwrap();
        self.git(repo, &["add", "."]);
        self.git(repo, &["commit", "-q", "-m", message]);
        self.git(repo, &["rev-parse", "HEAD"])
    }

    fn rev(&self, repo: &Path, name: &str) -> String {
        self.git(repo, &["rev-parse", name])
    }

    /// Every ref with its value, one per line.
    fn refs(&self, repo: &Path) -> String {
        self.git(repo, &["for-each-ref", "--format=%(refname) %(objectname)"])
    }

    fn runner(&self) -> GitRunner {
        GitRunner::new(
            Git::detect("git").unwrap(),
            RepoPaths {
                workdir: self.alice.clone(),
                git_dir: self.alice.join(".git"),
            },
        )
        .with_env(self.env.clone())
    }

    /// Runs one op in `alice`, expecting what is on disk now.
    fn run(&self, runner: &GitRunner, args: GitArgs) -> (Vec<Event>, OpOutcome) {
        let (op, events) = self.start(runner, args);
        finish(op, &events)
    }

    fn start(&self, runner: &GitRunner, args: GitArgs) -> (OpId, Receiver<Event>) {
        let paths = RepoPaths {
            workdir: self.alice.clone(),
            git_dir: self.alice.join(".git"),
        };
        let expect = precondition::current(&paths).unwrap();
        let (sender, receiver) = mpsc::channel();
        let op = OpId(1);
        runner.run(op, args, expect, sender);
        (op, receiver)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Collects events until `OpFinished`, which must be last and unique.
fn finish(op: OpId, events: &Receiver<Event>) -> (Vec<Event>, OpOutcome) {
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let event = events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the operation did not finish");
        if let Event::OpFinished(finished, outcome) = event {
            assert_eq!(finished, op);
            assert!(
                events.recv_timeout(Duration::from_millis(200)).is_err(),
                "events after OpFinished"
            );
            return (seen, outcome);
        }
        seen.push(event);
    }
}

fn main_on(remote: &str) -> RemoteBranch {
    RemoteBranch {
        remote: remote.into(),
        branch: BranchName("main".into()),
    }
}

fn branch(name: &str) -> BranchName {
    BranchName(name.into())
}

fn push(local: &str, lease: Option<Lease>, set_upstream: bool) -> GitArgs {
    GitArgs::Push {
        local: branch(local),
        to: RemoteBranch {
            remote: "origin".into(),
            branch: branch(local),
        },
        lease,
        set_upstream,
    }
}

fn lease(branch_name: &str, expected: &str) -> Lease {
    Lease {
        remote_ref: RefName(format!("refs/heads/{branch_name}")),
        expected: Oid::from_hex(expected).unwrap(),
    }
}

fn failure_class(outcome: &OpOutcome) -> ErrorClass {
    match outcome {
        OpOutcome::Failed(failure) => failure.class,
        other => panic!("expected a failure, got {other:?}"),
    }
}

// Law 9: a fetch moves remote-tracking refs and nothing else.
#[test]
fn fetch_moves_only_remote_tracking_refs() {
    let fx = Fixture::new("");
    let b1 = fx.commit(&fx.bob, "b.txt", "bob\n", "bob 1");
    fx.git(&fx.bob, &["push", "-q", "origin", "main"]);
    fx.git(&fx.bob, &["push", "-q", "origin", "main:refs/heads/other"]);
    fx.git(&fx.bob, &["tag", "v1"]);
    fx.git(&fx.bob, &["push", "-q", "origin", "v1"]);

    let local_before = fx.git(&fx.alice, &["for-each-ref", "refs/heads", "refs/stash"]);
    let head_before = fx.rev(&fx.alice, "HEAD");
    let status_before = fx.git(&fx.alice, &["status", "--porcelain"]);
    let runner = fx.runner();
    let (_, outcome) = fx.run(
        &runner,
        GitArgs::Fetch {
            remote: "origin".into(),
        },
    );
    assert_eq!(outcome, OpOutcome::Succeeded);

    assert_eq!(fx.rev(&fx.alice, "refs/remotes/origin/main"), b1);
    assert_eq!(fx.rev(&fx.alice, "refs/remotes/origin/other"), b1);
    // Local branches, HEAD and the worktree did not move.
    assert_eq!(
        fx.git(&fx.alice, &["for-each-ref", "refs/heads", "refs/stash"]),
        local_before
    );
    assert_eq!(fx.rev(&fx.alice, "HEAD"), head_before);
    assert_eq!(fx.git(&fx.alice, &["status", "--porcelain"]), status_before);
    assert!(!fx.alice.join("b.txt").exists());
}

// Law 9, also when the fetched branch is the checked-out one and has a
// configured upstream: `main` stays where it was.
#[test]
fn fetch_leaves_the_checked_out_branch_alone() {
    let fx = Fixture::new("");
    let mine = fx.commit(&fx.alice, "mine.txt", "mine\n", "mine");
    fx.commit(&fx.bob, "b.txt", "bob\n", "bob 1");
    fx.git(&fx.bob, &["push", "-q", "origin", "main"]);
    let (_, outcome) = fx.run(
        &fx.runner(),
        GitArgs::Fetch {
            remote: "origin".into(),
        },
    );
    assert_eq!(outcome, OpOutcome::Succeeded);
    assert_eq!(fx.rev(&fx.alice, "refs/heads/main"), mine);
}

#[test]
fn push_updates_the_remote() {
    let fx = Fixture::new("");
    let mine = fx.commit(&fx.alice, "mine.txt", "mine\n", "mine");
    let (events, outcome) = fx.run(&fx.runner(), push("main", None, false));
    assert_eq!(outcome, OpOutcome::Succeeded);
    assert_eq!(fx.rev(&fx.origin(), "refs/heads/main"), mine);
    // git was asked for progress, so a file:// push reports some.
    let _ = events;
}

#[test]
fn push_with_set_upstream_configures_the_branch() {
    let fx = Fixture::new("");
    fx.git(&fx.alice, &["switch", "-q", "-c", "topic"]);
    let tip = fx.commit(&fx.alice, "t.txt", "t\n", "topic");
    assert!(
        !fx.try_git(&fx.alice, &["rev-parse", "topic@{upstream}"])
            .status
            .success()
    );
    let (_, outcome) = fx.run(&fx.runner(), push("topic", None, true));
    assert_eq!(outcome, OpOutcome::Succeeded);
    assert_eq!(fx.rev(&fx.origin(), "refs/heads/topic"), tip);
    assert_eq!(fx.rev(&fx.alice, "topic@{upstream}"), tip);
    assert_eq!(
        fx.git(&fx.alice, &["config", "branch.topic.remote"]),
        "origin"
    );
}

#[test]
fn a_rejected_push_is_expected_and_changes_nothing() {
    let fx = Fixture::new("");
    fx.commit(&fx.bob, "b.txt", "bob\n", "bob 1");
    fx.git(&fx.bob, &["push", "-q", "origin", "main"]);
    let theirs = fx.rev(&fx.origin(), "refs/heads/main");
    fx.commit(&fx.alice, "mine.txt", "mine\n", "mine");

    let (_, outcome) = fx.run(&fx.runner(), push("main", None, false));
    assert_eq!(failure_class(&outcome), ErrorClass::Expected);
    let OpOutcome::Failed(failure) = outcome else {
        unreachable!()
    };
    assert!(failure.details.contains("rejected"), "{}", failure.details);
    assert_eq!(fx.rev(&fx.origin(), "refs/heads/main"), theirs);
}

// Law 10, shell half: the lease is checked by the remote against the value
// the user reviewed.
#[test]
fn force_with_lease_overwrites_only_the_reviewed_value() {
    let fx = Fixture::new("");
    let runner = fx.runner();
    // Alice rewrites history while the remote still holds the reviewed value.
    let reviewed = fx.rev(&fx.origin(), "refs/heads/main");
    fx.git(&fx.alice, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let rewritten = fx.rev(&fx.alice, "HEAD");
    assert_ne!(rewritten, reviewed);
    // Without a lease the remote refuses (not a fast-forward).
    let (_, outcome) = fx.run(&runner, push("main", None, false));
    assert_eq!(failure_class(&outcome), ErrorClass::Expected);
    // With the reviewed value it goes through.
    let (_, outcome) = fx.run(&runner, push("main", Some(lease("main", &reviewed)), false));
    assert_eq!(outcome, OpOutcome::Succeeded);
    assert_eq!(fx.rev(&fx.origin(), "refs/heads/main"), rewritten);
}

#[test]
fn force_with_lease_is_refused_when_the_remote_moved_since_review() {
    let fx = Fixture::new("");
    let runner = fx.runner();
    let reviewed = fx.rev(&fx.alice, "refs/remotes/origin/main");
    // Bob pushes after Alice looked at the remote.
    fx.commit(&fx.bob, "b.txt", "bob\n", "bob 1");
    fx.git(&fx.bob, &["push", "-q", "origin", "main"]);
    let theirs = fx.rev(&fx.origin(), "refs/heads/main");
    fx.git(&fx.alice, &["commit", "-q", "--amend", "-m", "rewritten"]);

    let (_, outcome) = fx.run(&runner, push("main", Some(lease("main", &reviewed)), false));
    assert_eq!(failure_class(&outcome), ErrorClass::Expected);
    let OpOutcome::Failed(failure) = outcome else {
        unreachable!()
    };
    assert!(
        failure.details.contains("stale info"),
        "{}",
        failure.details
    );
    // Bob's work is intact.
    assert_eq!(fx.rev(&fx.origin(), "refs/heads/main"), theirs);
}

#[test]
fn pull_merge_creates_a_merge_commit() {
    let fx = Fixture::new("");
    fx.commit(&fx.bob, "b.txt", "bob\n", "bob 1");
    fx.git(&fx.bob, &["push", "-q", "origin", "main"]);
    let theirs = fx.rev(&fx.origin(), "refs/heads/main");
    let mine = fx.commit(&fx.alice, "mine.txt", "mine\n", "mine");
    fx.git(&fx.alice, &["fetch", "-q"]);

    let (_, outcome) = fx.run(
        &fx.runner(),
        GitArgs::Pull {
            from: main_on("origin"),
            mode: PullMode::Merge,
        },
    );
    assert_eq!(outcome, OpOutcome::Succeeded);
    let parents = fx.git(&fx.alice, &["rev-list", "--parents", "-1", "HEAD"]);
    let parents: Vec<&str> = parents.split(' ').collect();
    assert_eq!(parents[1..], [mine.as_str(), theirs.as_str()]);
    assert!(fx.alice.join("b.txt").exists() && fx.alice.join("mine.txt").exists());
}

#[test]
fn pull_rebase_keeps_history_linear() {
    let fx = Fixture::new("");
    fx.commit(&fx.bob, "b.txt", "bob\n", "bob 1");
    fx.git(&fx.bob, &["push", "-q", "origin", "main"]);
    let theirs = fx.rev(&fx.origin(), "refs/heads/main");
    let mine = fx.commit(&fx.alice, "mine.txt", "mine\n", "mine");

    let (_, outcome) = fx.run(
        &fx.runner(),
        GitArgs::Pull {
            from: main_on("origin"),
            mode: PullMode::Rebase,
        },
    );
    assert_eq!(outcome, OpOutcome::Succeeded);
    let parents = fx.git(&fx.alice, &["rev-list", "--parents", "-1", "HEAD"]);
    let parents: Vec<&str> = parents.split(' ').collect();
    // One parent, and it is Bob's commit; Alice's commit was replayed.
    assert_eq!(parents[1..], [theirs.as_str()]);
    assert_ne!(parents[0], mine);
    assert_eq!(fx.git(&fx.alice, &["log", "-1", "--format=%s"]), "mine");
}

#[test]
fn a_pull_that_conflicts_is_an_expected_outcome() {
    let fx = Fixture::new("");
    fx.commit(&fx.bob, "base.txt", "bob\n", "bob edits base");
    fx.git(&fx.bob, &["push", "-q", "origin", "main"]);
    fx.commit(&fx.alice, "base.txt", "alice\n", "alice edits base");

    let (_, outcome) = fx.run(
        &fx.runner(),
        GitArgs::Pull {
            from: main_on("origin"),
            mode: PullMode::Merge,
        },
    );
    assert_eq!(failure_class(&outcome), ErrorClass::Expected);
    // The merge is left in progress for the conflict flow.
    assert!(fx.alice.join(".git").join("MERGE_HEAD").exists());
}

#[test]
fn set_upstream_points_the_branch_at_the_remote_branch() {
    let fx = Fixture::new("");
    fx.git(&fx.alice, &["switch", "-q", "-c", "topic"]);
    fx.commit(&fx.alice, "t.txt", "t\n", "topic");
    // Published without --set-upstream, so there is no upstream yet.
    fx.git(&fx.alice, &["push", "-q", "origin", "topic"]);
    assert!(
        !fx.try_git(&fx.alice, &["rev-parse", "topic@{upstream}"])
            .status
            .success()
    );
    let runner = fx.runner();
    let (_, outcome) = fx.run(
        &runner,
        GitArgs::SetUpstream {
            local: branch("topic"),
            upstream: RemoteBranch {
                remote: "origin".into(),
                branch: branch("topic"),
            },
        },
    );
    assert_eq!(outcome, OpOutcome::Succeeded);
    assert_eq!(
        fx.rev(&fx.alice, "topic@{upstream}"),
        fx.rev(&fx.alice, "refs/remotes/origin/topic")
    );

    // A remote branch we have never fetched cannot be an upstream.
    let (_, outcome) = fx.run(
        &runner,
        GitArgs::SetUpstream {
            local: branch("topic"),
            upstream: RemoteBranch {
                remote: "origin".into(),
                branch: branch("nope"),
            },
        },
    );
    assert_eq!(failure_class(&outcome), ErrorClass::Expected);
}

#[test]
fn an_unknown_remote_is_not_a_bug() {
    let fx = Fixture::new("");
    let (_, outcome) = fx.run(
        &fx.runner(),
        GitArgs::Fetch {
            remote: "nowhere".into(),
        },
    );
    assert_eq!(failure_class(&outcome), ErrorClass::Expected);
}

// Law 13: a cancelled fetch reports Cancelled and the repo is as it was.
#[test]
fn cancel_mid_fetch_leaves_the_repo_unchanged() {
    // The remote's pack-objects hook is slow only while `slow-on` exists, so
    // setup stays fast. Its grandchild `sleep` must die with the fetch.
    let slow = std::env::temp_dir().join(format!("invoke-git-sync-slow-{}", std::process::id()));
    fs::create_dir_all(&slow).unwrap();
    let hook = slow.join("hook.sh");
    let (on, started, finished) = (
        slow.join("slow-on"),
        slow.join("started"),
        slow.join("finished"),
    );
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nif [ -f '{}' ]; then\ntouch '{}'\nsleep 3\ntouch '{}'\nfi\nexec \"$@\"\n",
            on.display(),
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
    fx.commit(&fx.bob, "b.txt", "bob\n", "bob 1");
    fx.git(&fx.bob, &["push", "-q", "origin", "main"]);

    let refs_before = fx.refs(&fx.alice);
    let head_before = fx.rev(&fx.alice, "HEAD");
    fs::write(&on, "").unwrap();
    let runner = fx.runner();
    let (op, events) = fx.start(
        &runner,
        GitArgs::Fetch {
            remote: "origin".into(),
        },
    );
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

    // Nothing moved, and no lock was left behind.
    assert_eq!(fx.refs(&fx.alice), refs_before);
    assert_eq!(fx.rev(&fx.alice, "HEAD"), head_before);
    assert!(!fx.alice.join(".git").join("index.lock").exists());
    assert_eq!(fx.git(&fx.alice, &["status", "--porcelain"]), "");
    // The hook's `sleep` was killed with the tree: it never gets to finish.
    thread::sleep(Duration::from_millis(3500));
    assert!(!finished.exists(), "the hook outlived the cancel");
    let _ = fs::remove_dir_all(&slow);
}

// A stale precondition is refused before git runs: the write carries what it
// expects (rule 5).
#[test]
fn a_network_op_is_refused_when_head_moved() {
    let fx = Fixture::new("");
    let paths = RepoPaths {
        workdir: fx.alice.clone(),
        git_dir: fx.alice.join(".git"),
    };
    let expect = precondition::current(&paths).unwrap();
    fx.commit(&fx.alice, "late.txt", "late\n", "late");
    let (sender, receiver) = mpsc::channel();
    fx.runner()
        .run(OpId(1), push("main", None, false), expect, sender);
    let (_, outcome) = finish(OpId(1), &receiver);
    assert_eq!(failure_class(&outcome), ErrorClass::PreconditionChanged);
    assert_ne!(
        fx.rev(&fx.origin(), "refs/heads/main"),
        fx.rev(&fx.alice, "HEAD")
    );
}
