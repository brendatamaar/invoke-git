//! Undo end to end: the real core and the real shell on generated temp
//! repos, with real git as the oracle. System and global git config are
//! shut out, so the user's setup never leaks in. Owns law 8 (undo restores
//! refs and the index exactly) and law 9 (no other branch moves).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use invoke_core::{
    Action, ChangeSet, Command as Cmd, ErrorClass, Event, Expect, Notice, Oid, OpId, OpOutcome,
    Refusal, RepoId, RepoState, RestoreRef, UndoMoved, UndoReplay, open, step,
};
use shell::precondition::{self, RepoPaths};
use shell::undo::{Snapshotted, undo_ref};
use shell::{Git, GitRunner, UndoRunner, read::read_event};

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
            "invoke-git-undo-{}-{}-{nanos}",
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

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.paths.workdir.join(path)).unwrap()
    }

    fn stage(&self, path: &str, text: &str) {
        self.write(path, text);
        self.git(&["add", path]);
    }

    /// Everything undo promises to restore, as git reports it: HEAD, every
    /// ref except our own bookkeeping, and the index entries (mode, blob,
    /// stage, path) with the tree they make.
    fn observe(&self) -> Observed {
        let refs = self
            .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .lines()
            .filter(|line| !line.starts_with("refs/invoke-git/"))
            .map(str::to_owned)
            .collect();
        Observed {
            head: self.try_git(&["rev-parse", "-q", "--verify", "HEAD"]),
            refs,
            index_entries: self.git(&["ls-files", "-s"]),
            index_tree: self.index_tree(),
        }
    }

    /// The tree the index makes, written from a copy: a plain `git
    /// write-tree` rewrites the real index (cache-tree), which would change
    /// its id under the code being tested.
    fn index_tree(&self) -> String {
        let copy = self.root.join("index-copy");
        let _ = fs::copy(self.paths.git_dir.join("index"), &copy);
        let output = Command::new("git")
            .arg("write-tree")
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .env("GIT_INDEX_FILE", &copy)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn saved_refs(&self) -> Vec<String> {
        self.git(&["for-each-ref", "--format=%(refname)", "refs/invoke-git/"])
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    head: Option<String>,
    refs: BTreeSet<String>,
    index_entries: String,
    index_tree: String,
}

/// The real core driving the real shell, one command at a time.
struct Driver<'a> {
    repo: &'a TempRepo,
    state: RepoState,
    runner: GitRunner,
    undo: UndoRunner,
    /// Operations whose snapshot failed: their `RunGit` must not run.
    skip: BTreeSet<OpId>,
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
            skip: BTreeSet::new(),
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
                Snapshotted::Taken(event) => self.deliver(event),
                Snapshotted::Failed(event) => {
                    self.skip.insert(point.op);
                    self.deliver(event);
                }
            },
            Cmd::RunGit { op, args, expect } => {
                if self.skip.remove(&op) {
                    return;
                }
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

    fn commit(&mut self, message: &str, amend: bool) {
        self.deliver(Event::CommitRequested {
            message: message.into(),
            amend,
        });
        assert_eq!(self.last_outcome(), Some(OpOutcome::Succeeded), "commit");
    }

    fn undo(&mut self) {
        self.deliver(Event::UndoRequested);
    }

    /// What the watcher would send after someone else changed the repo.
    fn outside_change(&mut self) {
        self.deliver(Event::DiskChanged(ChangeSet(
            invoke_core::event::ALL_AREAS.into_iter().collect(),
        )));
    }

    fn last_outcome(&self) -> Option<OpOutcome> {
        match &self.state.notice {
            Some(Notice::OpEnded { outcome, .. }) => Some(outcome.clone()),
            _ => None,
        }
    }

    fn refusal(&self) -> Option<Refusal> {
        match &self.state.notice {
            Some(Notice::Refused {
                action: Action::Undo,
                reason,
            }) => Some(reason.clone()),
            _ => None,
        }
    }
}

fn seeded() -> TempRepo {
    let repo = TempRepo::new();
    repo.stage("a.txt", "one\n");
    repo.git(&["commit", "-q", "-m", "first"]);
    repo.git(&["branch", "dev"]);
    repo.git(&["tag", "v1"]);
    repo
}

/// Law 8 for commit: HEAD, the branch and the index come back exactly, and
/// the working tree is not touched.
#[test]
fn undo_commit_restores_head_branch_and_index() {
    let repo = seeded();
    repo.stage("a.txt", "two\n");
    repo.stage("b.txt", "new\n");
    repo.write("c.txt", "untracked\n");
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.commit("Second", false);
    let after_commit = repo.observe();
    assert_ne!(after_commit.head, before.head);
    assert_eq!(repo.saved_refs().len(), 1, "the saved tree is kept alive");
    assert_eq!(driver.state.undo.entries.len(), 1);

    driver.undo();
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(repo.observe(), before);
    assert!(repo.saved_refs().is_empty(), "the point is pruned with use");
    assert_eq!(repo.read("a.txt"), "two\n");
    assert_eq!(repo.read("c.txt"), "untracked\n");
    assert!(driver.state.undo.entries.is_empty());
    // The staged changes are staged again, as before.
    assert_eq!(
        repo.git(&["diff", "--cached", "--name-only"]),
        "a.txt\nb.txt"
    );
}

/// Law 8 for amend: back to the commit that was amended, with the index
/// that the amend consumed.
#[test]
fn undo_amend_restores_the_amended_commit() {
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    repo.stage("a.txt", "two\n");
    driver.outside_change();
    driver.commit("Second", false);
    repo.stage("a.txt", "three\n");
    driver.outside_change();
    let before = repo.observe();

    driver.commit("Second, reworded", true);
    assert_ne!(repo.observe().head, before.head);
    driver.undo();
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(repo.observe(), before);
    assert_eq!(repo.git(&["log", "-1", "--format=%s"]), "Second");
}

/// Law 9: committing and undoing moves only the current branch.
#[test]
fn undo_moves_no_other_branch() {
    let repo = seeded();
    repo.git(&["branch", "topic"]);
    repo.git(&["tag", "-a", "v2", "-m", "annotated"]);
    let mut driver = Driver::open(&repo);
    repo.stage("a.txt", "two\n");
    driver.outside_change();
    let before = repo.observe();

    driver.commit("Second", false);
    let now = repo.observe();
    let moved: Vec<_> = before.refs.symmetric_difference(&now.refs).collect();
    assert_eq!(moved.len(), 2, "only main changed: {moved:?}");
    assert!(
        moved
            .iter()
            .all(|line| line.starts_with("refs/heads/main "))
    );

    driver.undo();
    assert_eq!(repo.observe(), before);
}

/// Several undos walk back through several operations and prune as they go.
#[test]
fn chained_undos_walk_back_to_the_start() {
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    let start = repo.observe();
    repo.stage("a.txt", "two\n");
    driver.outside_change();
    let one = repo.observe();
    driver.commit("Second", false);
    let two_staged = {
        repo.stage("a.txt", "three\n");
        driver.outside_change();
        repo.observe()
    };
    driver.commit("Third", false);
    assert_eq!(repo.saved_refs().len(), 2);

    driver.undo();
    assert_eq!(repo.observe(), two_staged);
    driver.undo();
    assert_eq!(repo.observe(), one);
    assert_ne!(one, start);
    assert!(repo.saved_refs().is_empty());
    driver.undo();
    assert_eq!(driver.refusal(), Some(Refusal::NothingToUndo));
}

/// The first commit of a repo: undo deletes the branch again.
#[test]
fn undo_the_first_commit_makes_the_branch_unborn_again() {
    let repo = TempRepo::new();
    repo.stage("a.txt", "one\n");
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.commit("First", false);
    assert!(repo.observe().head.is_some());
    driver.undo();
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(repo.observe(), before);
    assert_eq!(repo.try_git(&["rev-parse", "-q", "--verify", "HEAD"]), None);
}

/// Someone else committed after ours: undo refuses and changes nothing.
#[test]
fn undo_is_refused_after_an_outside_commit() {
    let repo = seeded();
    repo.stage("a.txt", "two\n");
    let mut driver = Driver::open(&repo);
    driver.commit("Second", false);
    repo.git(&["commit", "-q", "--allow-empty", "-m", "outside"]);
    driver.outside_change();
    let before = repo.observe();

    driver.undo();
    assert_eq!(
        driver.refusal(),
        Some(Refusal::Changed(UndoMoved::Ref(
            invoke_core::BranchName("main".into()).to_ref()
        )))
    );
    assert_eq!(repo.observe(), before);
    assert_eq!(repo.git(&["log", "-1", "--format=%s"]), "outside");
    assert_eq!(driver.state.undo.entries.len(), 1, "the point stays");
}

/// The user staged something after the commit: undo would overwrite it.
#[test]
fn undo_is_refused_after_the_index_changed() {
    let repo = seeded();
    repo.stage("a.txt", "two\n");
    let mut driver = Driver::open(&repo);
    driver.commit("Second", false);
    repo.stage("late.txt", "late\n");
    driver.outside_change();
    let before = repo.observe();

    driver.undo();
    assert_eq!(driver.refusal(), Some(Refusal::Changed(UndoMoved::Index)));
    assert_eq!(repo.observe(), before);
}

/// Switching branch after the commit: undo refuses rather than rewrite the
/// other branch's index.
#[test]
fn undo_is_refused_after_switching_branch() {
    let repo = seeded();
    repo.stage("a.txt", "two\n");
    let mut driver = Driver::open(&repo);
    driver.commit("Second", false);
    repo.git(&["switch", "-q", "dev"]);
    driver.outside_change();
    let before = repo.observe();

    driver.undo();
    assert_eq!(driver.refusal(), Some(Refusal::Changed(UndoMoved::Head)));
    assert_eq!(repo.observe(), before);
}

/// A commit that fails leaves no point and no saved ref. Task 14: nothing
/// staged is refused by the core before anything is saved, so the failing
/// run here is a commit hook that rejects it.
#[test]
fn a_failed_commit_leaves_nothing_behind() {
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    driver.deliver(Event::CommitRequested {
        message: "empty".into(),
        amend: false,
    });
    assert!(driver.last_outcome().is_none());
    assert!(driver.state.undo.entries.is_empty());
    assert!(repo.saved_refs().is_empty());

    let hooks = repo.paths.git_dir.join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    repo.stage("a.txt", "two\n");
    driver.outside_change();
    driver.deliver(Event::CommitRequested {
        message: "rejected".into(),
        amend: false,
    });
    assert!(matches!(driver.last_outcome(), Some(OpOutcome::Failed(_))));
    assert!(driver.state.undo.entries.is_empty());
    assert!(repo.saved_refs().is_empty());
    driver.undo();
    assert_eq!(driver.refusal(), Some(Refusal::NothingToUndo));
}

/// The saved tree is a real tree object under `refs/invoke-git/undo/<op>`,
/// the snapshot leaves the real index (and its id) alone, and a snapshot of
/// an index that moved is refused.
#[test]
fn snapshot_keeps_a_tree_alive_without_touching_the_index() {
    let repo = seeded();
    repo.stage("a.txt", "two\n");
    let driver = Driver::open(&repo);
    let disk_before = precondition::current(&repo.paths).unwrap();
    let tree_before = repo.index_tree();
    let point = invoke_core::UndoPoint {
        op: OpId(40),
        label: "x".into(),
        head: invoke_core::HeadTarget::Branch(invoke_core::BranchName("main".into())),
        refs: vec![],
        index: disk_before.index.clone(),
        index_effect: invoke_core::IndexEffect::Rewrites,
        reverse: invoke_core::Reversal::Refs,
    };

    let Snapshotted::Taken(Event::UndoSnapshotTaken { op, tree }) = driver.undo.snapshot(&point)
    else {
        panic!("snapshot failed");
    };
    assert_eq!(op, OpId(40));
    assert_eq!(tree.as_hex(), tree_before);
    assert_eq!(precondition::current(&repo.paths).unwrap(), disk_before);
    let kept = undo_ref(OpId(40));
    assert_eq!(repo.git(&["cat-file", "-t", &kept]), "tree");
    assert_eq!(repo.git(&["rev-parse", &kept]), tree_before);
    assert!(
        !repo.paths.git_dir.join("invoke-undo-index-40").exists(),
        "the private index copy is removed"
    );

    // The index moves: a snapshot against the old id is refused.
    repo.stage("b.txt", "b\n");
    let Snapshotted::Failed(Event::OpFinished(OpId(40), OpOutcome::Failed(failure))) =
        driver.undo.snapshot(&point)
    else {
        panic!("snapshot should be refused");
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);

    driver.undo.prune(&[OpId(40), OpId(41)]);
    assert!(repo.saved_refs().is_empty());
}

/// The replay is atomic against races: a ref that is not where the plan said
/// stops the whole replay, and neither refs nor index change.
#[test]
fn replay_refuses_when_a_ref_is_not_where_planned() {
    let repo = seeded();
    repo.stage("a.txt", "two\n");
    repo.git(&["commit", "-q", "-m", "second"]);
    let first = repo.git(&["rev-parse", "HEAD~1"]);
    let second = repo.git(&["rev-parse", "HEAD"]);
    let tree = repo.index_tree();
    repo.stage("b.txt", "b\n");
    let before = repo.observe();
    let driver = Driver::open(&repo);
    let oid = |hex: &str| Oid::from_hex(hex).unwrap();
    let replay = |planned: &str| UndoReplay {
        op: OpId(50),
        refs: vec![RestoreRef {
            name: invoke_core::BranchName("main".into()).to_ref(),
            expect: Some(oid(planned)),
            restore: Some(oid(&first)),
        }],
        tree: oid(&tree),
        index_effect: invoke_core::IndexEffect::Rewrites,
        head: None,
        expect: precondition::current(&repo.paths).unwrap(),
        stash: None,
    };

    // Planned against `first`, but main is at `second`.
    let Event::OpFinished(_, OpOutcome::Failed(failure)) = driver.undo.replay(&replay(&first))
    else {
        panic!("replay should fail");
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    assert_eq!(repo.observe(), before);

    // Planned correctly, it goes through.
    assert_eq!(
        driver.undo.replay(&replay(&second)),
        Event::OpFinished(OpId(50), OpOutcome::Succeeded)
    );
    assert_eq!(repo.git(&["rev-parse", "main"]), first);
    assert_eq!(repo.index_tree(), tree);
}

/// A stale `Expect` (HEAD or index moved since the plan) stops a replay
/// before anything is written.
#[test]
fn replay_refuses_a_stale_expect() {
    let repo = seeded();
    repo.stage("a.txt", "two\n");
    let tree = repo.index_tree();
    let head = repo.git(&["rev-parse", "HEAD"]);
    let stale = Expect {
        index: invoke_core::IndexId::Absent,
        ..precondition::current(&repo.paths).unwrap()
    };
    let before = repo.observe();
    let driver = Driver::open(&repo);
    let outcome = driver.undo.replay(&UndoReplay {
        op: OpId(51),
        refs: vec![RestoreRef {
            name: invoke_core::BranchName("main".into()).to_ref(),
            expect: Some(Oid::from_hex(&head).unwrap()),
            restore: Some(Oid::from_hex(&head).unwrap()),
        }],
        tree: Oid::from_hex(&tree).unwrap(),
        index_effect: invoke_core::IndexEffect::Rewrites,
        head: None,
        expect: stale,
        stash: None,
    });
    assert!(matches!(
        outcome,
        Event::OpFinished(_, OpOutcome::Failed(f)) if f.class == ErrorClass::PreconditionChanged
    ));
    assert_eq!(repo.observe(), before);
}
