//! Branch operations (task 15) end to end: the real core driving the real
//! shell on generated temp repos, with real git as the oracle. System and
//! global git config are shut out. Owns law 9 (no untargeted branch moves)
//! for these operations and law 8 (undo restores every ref, HEAD and the
//! index) for create, switch, rename and delete.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use invoke_core::{
    Action, BranchName, BranchRefusal, BranchStart, ChangeSet, Command as Cmd, ErrorClass, Event,
    GitArgs, Notice, Oid, OpId, OpOutcome, Refusal, RepoId, RepoState, Tracking, UndoMoved, open,
    step,
};
use proptest::prelude::*;
use shell::classify::is_unmerged_refusal;
use shell::precondition::{self, RepoPaths};
use shell::undo::Snapshotted;
use shell::{Git, GitRunner, UndoRunner, read::read_event};

struct TempRepo {
    root: PathBuf,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
}

impl TempRepo {
    fn new() -> Self {
        Self::named("repo")
    }

    fn named(dir: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let root = std::env::temp_dir().join(format!(
            "invoke-git-branches-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let workdir = root.join(dir);
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

    fn stage(&self, path: &str, text: &str) {
        self.write(path, text);
        self.git(&["add", path]);
    }

    fn commit_file(&self, path: &str, text: &str, message: &str) {
        self.stage(path, text);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn rev(&self, name: &str) -> String {
        self.git(&["rev-parse", name])
    }

    /// The index as a tree, written from a copy: a plain `git write-tree`
    /// rewrites the real index and would change its id under the code
    /// being tested.
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

    /// Everything these operations may change, as git reports it: HEAD
    /// (the branch it names, or the commit when detached), every ref but
    /// our bookkeeping, the index and the files in the worktree.
    fn observe(&self) -> Observed {
        let refs = self
            .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .lines()
            .filter(|line| !line.starts_with("refs/invoke-git/"))
            .map(|line| {
                let (name, oid) = line.split_once(' ').unwrap();
                (name.to_owned(), oid.to_owned())
            })
            .collect();
        let files = self
            .git(&["ls-files", "-co", "--exclude-standard"])
            .lines()
            .map(|path| {
                let text = fs::read_to_string(self.paths.workdir.join(path)).unwrap_or_default();
                (path.to_owned(), text)
            })
            .collect();
        Observed {
            head_branch: self.try_git(&["symbolic-ref", "-q", "HEAD"]),
            head: self.try_git(&["rev-parse", "-q", "--verify", "HEAD"]),
            refs,
            index_entries: self.git(&["ls-files", "-s"]),
            index_tree: self.index_tree(),
            files,
        }
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
    head_branch: Option<String>,
    head: Option<String>,
    refs: BTreeMap<String, String>,
    index_entries: String,
    index_tree: String,
    files: BTreeMap<String, String>,
}

/// The refs that differ between two observations (added, removed, moved).
fn moved(before: &Observed, after: &Observed) -> BTreeSet<String> {
    before
        .refs
        .keys()
        .chain(after.refs.keys())
        .filter(|name| before.refs.get(*name) != after.refs.get(*name))
        .cloned()
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A repo with: `main` (c1, c2), `old` at c1 (merged into main), and
/// `feature` (c2 plus a commit of its own, not merged). HEAD is `main`.
fn seeded() -> TempRepo {
    let repo = TempRepo::new();
    repo.stage("a.txt", "one\n");
    repo.stage("b.txt", "b\n");
    repo.git(&["commit", "-q", "-m", "c1"]);
    repo.commit_file("a.txt", "two\n", "c2");
    repo.git(&["branch", "old", "HEAD~1"]);
    repo.git(&["switch", "-q", "-c", "feature"]);
    repo.commit_file("f.txt", "feature\n", "c3");
    repo.git(&["switch", "-q", "main"]);
    repo
}

struct Driver<'a> {
    repo: &'a TempRepo,
    state: RepoState,
    runner: GitRunner,
    undo: UndoRunner,
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

    /// What the watcher would send after someone else changed the repo.
    fn outside_change(&mut self) {
        self.deliver(Event::DiskChanged(ChangeSet(
            invoke_core::event::ALL_AREAS.into_iter().collect(),
        )));
    }

    fn create(&mut self, name: &str, at: BranchStart, switch: bool) {
        self.deliver(Event::BranchCreateRequested {
            name: name.into(),
            at,
            switch,
        });
    }

    fn switch(&mut self, name: &str) {
        self.deliver(Event::BranchSwitchRequested(BranchName(name.into())));
    }

    fn rename(&mut self, from: &str, to: &str) {
        self.deliver(Event::BranchRenameRequested {
            from: BranchName(from.into()),
            to: to.into(),
        });
    }

    fn delete(&mut self, name: &str, force: bool) {
        self.deliver(Event::BranchDeleteRequested {
            name: BranchName(name.into()),
            force,
        });
    }

    fn undo(&mut self) {
        self.deliver(Event::UndoRequested);
    }

    fn outcome(&self) -> Option<OpOutcome> {
        match &self.state.notice {
            Some(Notice::OpEnded { outcome, .. }) => Some(outcome.clone()),
            _ => None,
        }
    }

    fn succeeded(&self) -> bool {
        self.outcome() == Some(OpOutcome::Succeeded)
    }

    fn failure_class(&self) -> Option<ErrorClass> {
        match self.outcome() {
            Some(OpOutcome::Failed(failure)) => Some(failure.class),
            _ => None,
        }
    }

    fn refusal(&self) -> Option<(Action, Refusal)> {
        match &self.state.notice {
            Some(Notice::Refused { action, reason }) => Some((*action, reason.clone())),
            _ => None,
        }
    }
}

fn oid(hex: &str) -> Oid {
    Oid::from_hex(hex).unwrap()
}

// ---------------------------------------------------------------- create

/// Law 9 and law 8 for a plain create: only the new ref appears, HEAD, the
/// index and the files are untouched, and undo removes it again.
#[test]
fn create_from_head_then_undo() {
    let repo = seeded();
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.create("topic", BranchStart::Head, false);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = repo.observe();
    assert_eq!(moved(&before, &after), set(&["refs/heads/topic"]));
    assert_eq!(after.refs["refs/heads/topic"], repo.rev("main"));
    assert_eq!(after.head_branch, before.head_branch);
    assert_eq!(after.index_entries, before.index_entries);
    assert_eq!(repo.saved_refs().len(), 1);

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
    assert!(repo.saved_refs().is_empty());
}

#[test]
fn create_from_a_given_commit() {
    let repo = seeded();
    let before = repo.observe();
    let first = repo.rev("old");
    let mut driver = Driver::open(&repo);

    driver.create("topic", BranchStart::Commit(oid(&first)), false);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = repo.observe();
    assert_eq!(moved(&before, &after), set(&["refs/heads/topic"]));
    assert_eq!(after.refs["refs/heads/topic"], first);
    // The new branch does not track anything, whatever the start was.
    assert_eq!(repo.try_git(&["config", "branch.topic.merge"]), None);

    driver.undo();
    assert_eq!(repo.observe(), before);
}

/// A commit that does not exist is refused by git, as an expected failure,
/// and nothing is left behind.
#[test]
fn create_from_a_missing_commit_fails_cleanly() {
    let repo = seeded();
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.create("topic", BranchStart::Commit(oid(&"1".repeat(40))), false);
    assert_eq!(driver.failure_class(), Some(ErrorClass::Expected));
    assert_eq!(repo.observe(), before);
    assert!(repo.saved_refs().is_empty());
    assert!(driver.state.undo.entries.is_empty());
}

#[test]
fn create_and_switch_then_undo() {
    let repo = seeded();
    repo.write("scratch.txt", "untracked\n");
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.create("topic", BranchStart::Head, true);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = repo.observe();
    assert_eq!(moved(&before, &after), set(&["refs/heads/topic"]));
    assert_eq!(after.head_branch.as_deref(), Some("refs/heads/topic"));

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
}

/// Creating at another commit and switching rewrites files and index; undo
/// puts the branch, HEAD, index and files back.
#[test]
fn create_and_switch_at_another_commit_then_undo() {
    let repo = seeded();
    repo.write("dirty.txt", "carried\n");
    let before = repo.observe();
    let first = repo.rev("old");
    let mut driver = Driver::open(&repo);

    driver.create("topic", BranchStart::Commit(oid(&first)), true);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = repo.observe();
    assert_eq!(after.files["a.txt"], "one\n");
    assert_eq!(moved(&before, &after), set(&["refs/heads/topic"]));

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
    assert_eq!(repo.observe().files["a.txt"], "two\n");
}

/// Leaving a detached HEAD by creating a branch where it is: the work is
/// not lost, and undo detaches again at the same commit.
#[test]
fn leave_detached_head_by_creating_a_branch() {
    let repo = seeded();
    repo.git(&["switch", "-q", "--detach", "old"]);
    let before = repo.observe();
    assert_eq!(before.head_branch, None);
    let mut driver = Driver::open(&repo);

    driver.create("rescue", BranchStart::Head, true);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = repo.observe();
    assert_eq!(after.head_branch.as_deref(), Some("refs/heads/rescue"));
    assert_eq!(after.refs["refs/heads/rescue"], repo.rev("old"));
    assert_eq!(moved(&before, &after), set(&["refs/heads/rescue"]));

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
}

/// Nothing is created over an existing branch, or with a name that clashes
/// with one (`a` and `a/b`); the core says why and nothing runs.
#[test]
fn create_is_refused_for_existing_and_clashing_names() {
    let repo = seeded();
    repo.git(&["branch", "team/x"]);
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.create("old", BranchStart::Head, false);
    assert_eq!(
        driver.refusal(),
        Some((
            Action::CreateBranch,
            Refusal::Branch(BranchRefusal::AlreadyExists(BranchName("old".into())))
        ))
    );
    driver.create("team", BranchStart::Head, false);
    assert_eq!(
        driver.refusal(),
        Some((
            Action::CreateBranch,
            Refusal::Branch(BranchRefusal::NameConflict(BranchName("team/x".into())))
        ))
    );
    driver.create("bad name", BranchStart::Head, false);
    assert!(matches!(
        driver.refusal(),
        Some((
            Action::CreateBranch,
            Refusal::Branch(BranchRefusal::InvalidName(_))
        ))
    ));
    assert_eq!(repo.observe(), before);
    assert!(repo.saved_refs().is_empty());
}

// ---------------------------------------------------------------- switch

/// Switching moves HEAD, the index and the files, and no branch (law 9).
/// Local changes that do not clash come along; undo brings everything back.
#[test]
fn switch_then_undo() {
    let repo = seeded();
    repo.write("b.txt", "local edit\n");
    repo.write("scratch.txt", "untracked\n");
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.switch("feature");
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = repo.observe();
    assert_eq!(after.head_branch.as_deref(), Some("refs/heads/feature"));
    assert_eq!(after.files["f.txt"], "feature\n");
    assert_eq!(after.files["b.txt"], "local edit\n");
    assert_eq!(moved(&before, &after), BTreeSet::new(), "a branch moved");

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
}

#[test]
fn switch_to_an_older_branch_and_back_with_undo() {
    let repo = seeded();
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.switch("old");
    assert!(driver.succeeded());
    assert_eq!(repo.observe().files["a.txt"], "one\n");
    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
    assert_eq!(repo.observe().files["a.txt"], "two\n");
}

/// A detached HEAD may switch to a branch (PRD "State machines"); undo
/// detaches again at the same commit.
#[test]
fn switch_away_from_a_detached_head_then_undo() {
    let repo = seeded();
    repo.git(&["switch", "-q", "--detach", "old"]);
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.switch("main");
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(
        repo.observe().head_branch.as_deref(),
        Some("refs/heads/main")
    );

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
}

/// Decision: a switch that git would refuse (local changes that would be
/// overwritten) is left to git. It refuses before changing anything; the
/// shell classifies it as an expected failure and no undo point is kept.
#[test]
fn switch_over_clashing_local_changes_is_refused_by_git() {
    let repo = seeded();
    repo.write("a.txt", "local edit\n");
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.switch("old");
    assert_eq!(driver.failure_class(), Some(ErrorClass::Expected));
    assert_eq!(repo.observe(), before);
    assert!(repo.saved_refs().is_empty());
    assert!(driver.state.undo.entries.is_empty());
}

#[test]
fn switch_is_refused_for_the_current_and_missing_branches() {
    let repo = seeded();
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.switch("main");
    assert_eq!(
        driver.refusal(),
        Some((
            Action::SwitchBranch,
            Refusal::Branch(BranchRefusal::IsCurrent(BranchName("main".into())))
        ))
    );
    driver.switch("nope");
    assert_eq!(
        driver.refusal(),
        Some((
            Action::SwitchBranch,
            Refusal::Branch(BranchRefusal::NoSuchBranch(BranchName("nope".into())))
        ))
    );
    assert_eq!(repo.observe(), before);
}

/// `switch` never turns a remote branch into a local one behind the
/// user's back: the core names local branches only.
#[test]
fn switch_does_not_guess_a_remote_branch() {
    let repo = seeded();
    repo.git(&["update-ref", "refs/remotes/origin/guess", "main"]);
    repo.git(&["config", "remote.origin.url", "."]);
    repo.git(&[
        "config",
        "remote.origin.fetch",
        "+refs/heads/*:refs/remotes/origin/*",
    ]);
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.switch("guess");
    assert!(matches!(
        driver.refusal(),
        Some((
            Action::SwitchBranch,
            Refusal::Branch(BranchRefusal::NoSuchBranch(_))
        ))
    ));
    assert_eq!(repo.observe(), before);
}

// ---------------------------------------------------------------- rename

/// Law 9 and 8: renaming moves exactly the old and new names; undo puts the
/// old one back at the same commit.
#[test]
fn rename_another_branch_then_undo() {
    let repo = seeded();
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.rename("old", "ancient");
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = repo.observe();
    assert_eq!(
        moved(&before, &after),
        set(&["refs/heads/old", "refs/heads/ancient"])
    );
    assert_eq!(
        after.refs["refs/heads/ancient"],
        before.refs["refs/heads/old"]
    );
    assert_eq!(after.head_branch, before.head_branch);

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
}

/// Renaming the checked-out branch: HEAD follows, and undo points HEAD back
/// at the old name without touching a file.
#[test]
fn rename_the_current_branch_then_undo() {
    let repo = seeded();
    repo.write("scratch.txt", "untracked\n");
    repo.write("b.txt", "local edit\n");
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.rename("main", "trunk");
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = repo.observe();
    assert_eq!(after.head_branch.as_deref(), Some("refs/heads/trunk"));
    assert_eq!(
        moved(&before, &after),
        set(&["refs/heads/main", "refs/heads/trunk"])
    );

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
}

#[test]
fn rename_is_refused_for_taken_same_and_missing_names() {
    let repo = seeded();
    let before = repo.observe();
    let mut driver = Driver::open(&repo);
    let branch = |name: &str| BranchName(name.into());

    driver.rename("old", "feature");
    assert_eq!(
        driver.refusal(),
        Some((
            Action::RenameBranch,
            Refusal::Branch(BranchRefusal::AlreadyExists(branch("feature")))
        ))
    );
    driver.rename("old", "old");
    assert_eq!(
        driver.refusal(),
        Some((
            Action::RenameBranch,
            Refusal::Branch(BranchRefusal::SameName)
        ))
    );
    driver.rename("nope", "x");
    assert_eq!(
        driver.refusal(),
        Some((
            Action::RenameBranch,
            Refusal::Branch(BranchRefusal::NoSuchBranch(branch("nope")))
        ))
    );
    assert_eq!(repo.observe(), before);
}

// ---------------------------------------------------------------- delete

/// Law 9 and 8: deleting a merged branch moves only that ref; undo brings
/// it back at the same commit.
#[test]
fn delete_a_merged_branch_then_undo() {
    let repo = seeded();
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.delete("old", false);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = repo.observe();
    assert_eq!(moved(&before, &after), set(&["refs/heads/old"]));
    assert!(!after.refs.contains_key("refs/heads/old"));

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
}

/// An unmerged branch is not deleted without force: git refuses, the shell
/// classifies it as expected and the details name the reason; forcing it
/// works and undo restores the branch with its commits.
#[test]
fn delete_of_an_unmerged_branch_needs_force() {
    let repo = seeded();
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    driver.delete("feature", false);
    assert_eq!(driver.failure_class(), Some(ErrorClass::Expected));
    let Some(OpOutcome::Failed(failure)) = driver.outcome() else {
        panic!();
    };
    assert!(is_unmerged_refusal(&failure.details), "{}", failure.details);
    assert_eq!(repo.observe(), before);
    assert!(repo.saved_refs().is_empty(), "the failed point is dropped");
    assert!(driver.state.undo.entries.is_empty());

    driver.delete("feature", true);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = repo.observe();
    assert_eq!(moved(&before, &after), set(&["refs/heads/feature"]));

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.observe(), before);
}

/// Deleting the current branch is refused by the core, forced or not.
#[test]
fn deleting_the_current_branch_is_refused() {
    let repo = seeded();
    let before = repo.observe();
    let mut driver = Driver::open(&repo);

    for force in [false, true] {
        driver.delete("main", force);
        assert_eq!(
            driver.refusal(),
            Some((
                Action::DeleteBranch,
                Refusal::Branch(BranchRefusal::IsCurrent(BranchName("main".into())))
            ))
        );
        assert_eq!(repo.observe(), before);
    }
    assert!(repo.saved_refs().is_empty());
}

// ------------------------------------------------- stale expected values

/// Rule 5: a branch that moved since the core read it stops the write. The
/// disk is the source of truth; nothing is changed and no point is kept.
#[test]
fn delete_is_refused_when_the_branch_moved_since_the_snapshot() {
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    // Someone else moves `old` and the watcher has not reported it yet.
    repo.git(&["branch", "-f", "old", "feature"]);
    let before = repo.observe();

    driver.delete("old", true);
    assert_eq!(
        driver.failure_class(),
        Some(ErrorClass::PreconditionChanged)
    );
    assert_eq!(repo.observe(), before);
    assert!(repo.saved_refs().is_empty());
    assert!(driver.state.undo.entries.is_empty());
}

#[test]
fn every_branch_write_refuses_a_stale_expected_ref() {
    let repo = seeded();
    let main = oid(&repo.rev("main"));
    let wrong = oid(&repo.rev("old"));
    let before = repo.observe();
    let head = precondition::current(&repo.paths).unwrap();
    let branch = |name: &str| BranchName(name.into());
    let cases = [
        // Expected to be absent, but exists.
        GitArgs::CreateBranch {
            name: branch("old"),
            at: main.clone(),
            switch: false,
        },
        GitArgs::CreateBranch {
            name: branch("feature"),
            at: main.clone(),
            switch: true,
        },
        // Expected somewhere else.
        GitArgs::SwitchBranch {
            name: branch("feature"),
            tip: wrong.clone(),
        },
        GitArgs::DeleteBranch {
            name: branch("feature"),
            tip: wrong.clone(),
            force: true,
        },
        GitArgs::RenameBranch {
            from: branch("feature"),
            tip: wrong.clone(),
            to: branch("fresh"),
        },
        // Target expected to be free, but is not.
        GitArgs::RenameBranch {
            from: branch("old"),
            tip: wrong.clone(),
            to: branch("feature"),
        },
        // A branch that is gone.
        GitArgs::DeleteBranch {
            name: branch("ghost"),
            tip: main.clone(),
            force: true,
        },
    ];
    let runner =
        GitRunner::new(Git::detect("git").unwrap(), repo.paths.clone()).with_env(repo.env.clone());
    for (n, args) in cases.into_iter().enumerate() {
        let (sender, events) = mpsc::channel();
        runner.run(OpId(n as u64), args.clone(), head.clone(), sender);
        let outcome = loop {
            match events.recv_timeout(Duration::from_secs(30)).unwrap() {
                Event::OpFinished(_, outcome) => break outcome,
                _ => continue,
            }
        };
        let OpOutcome::Failed(failure) = outcome else {
            panic!("{args:?} ran on a stale expectation");
        };
        assert_eq!(failure.class, ErrorClass::PreconditionChanged, "{args:?}");
        // Our own check stopped it, not whatever git would have said.
        assert!(
            failure.details.contains("moved: expected"),
            "{args:?}: {}",
            failure.details
        );
        assert_eq!(repo.observe(), before, "{args:?} changed the repo");
    }
}

// ------------------------------------------------------------------ undo

/// Undo never overwrites someone else's change: a recorded branch that
/// moved, or a HEAD that is somewhere else, makes it refuse with a reason.
#[test]
fn undo_is_refused_after_outside_changes() {
    // The new branch was moved by someone else.
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    driver.create("topic", BranchStart::Head, false);
    repo.git(&["branch", "-f", "topic", "old"]);
    driver.outside_change();
    let before = repo.observe();
    driver.undo();
    assert_eq!(
        driver.refusal(),
        Some((
            Action::Undo,
            Refusal::Changed(UndoMoved::Ref(BranchName("topic".into()).to_ref()))
        ))
    );
    assert_eq!(repo.observe(), before);

    // HEAD was switched by someone else after a rename.
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    driver.rename("old", "ancient");
    repo.git(&["switch", "-q", "feature"]);
    driver.outside_change();
    let before = repo.observe();
    driver.undo();
    assert_eq!(
        driver.refusal(),
        Some((Action::Undo, Refusal::Changed(UndoMoved::Head)))
    );
    assert_eq!(repo.observe(), before);

    // A switch whose old branch moved on.
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    driver.switch("feature");
    repo.git(&["branch", "-f", "main", "old"]);
    driver.outside_change();
    let before = repo.observe();
    driver.undo();
    assert_eq!(
        driver.refusal(),
        Some((
            Action::Undo,
            Refusal::Changed(UndoMoved::Ref(BranchName("main".into()).to_ref()))
        ))
    );
    assert_eq!(repo.observe(), before);
}

/// Creating a branch never touches the index, so undoing it must not wipe
/// staging done since.
#[test]
fn undoing_a_create_keeps_later_staging() {
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    driver.create("topic", BranchStart::Head, false);
    repo.stage("later.txt", "staged after\n");
    driver.outside_change();

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert!(
        repo.try_git(&["rev-parse", "-q", "--verify", "topic"])
            .is_none()
    );
    assert_eq!(repo.git(&["diff", "--cached", "--name-only"]), "later.txt");
}

/// A chain of operations walks back one undo at a time, each restoring the
/// state before it.
#[test]
fn a_chain_of_branch_operations_unwinds() {
    let repo = seeded();
    let mut driver = Driver::open(&repo);
    let mut seen = vec![repo.observe()];

    driver.create("topic", BranchStart::Head, true);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    seen.push(repo.observe());
    driver.rename("topic", "topic2");
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    seen.push(repo.observe());
    driver.switch("feature");
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    seen.push(repo.observe());
    driver.delete("old", false);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);

    for expected in seen.iter().rev() {
        driver.undo();
        assert!(driver.succeeded(), "{:?}", driver.state.notice);
        assert_eq!(&repo.observe(), expected);
    }
    assert!(repo.saved_refs().is_empty());
}

/// A replay that has to move HEAD checks every ref first, so a stale one is
/// found before any checkout happens (no reflog entry is written).
#[test]
fn replay_with_a_head_move_checks_refs_before_moving_head() {
    let repo = seeded();
    let driver = Driver::open(&repo);
    let reflog = repo.git(&["reflog", "show", "--format=%H", "HEAD"]);
    let before = repo.observe();
    let replay = invoke_core::UndoReplay {
        op: OpId(60),
        refs: vec![invoke_core::RestoreRef {
            name: BranchName("old".into()).to_ref(),
            // `old` is not where this says.
            expect: Some(oid(&repo.rev("feature"))),
            restore: Some(oid(&repo.rev("main"))),
        }],
        tree: oid(&repo.index_tree()),
        index_effect: invoke_core::IndexEffect::Untouched,
        head: Some(invoke_core::HeadRestore::Branch(BranchName(
            "feature".into(),
        ))),
        expect: precondition::current(&repo.paths).unwrap(),
        stash: None,
    };
    let Event::OpFinished(_, OpOutcome::Failed(failure)) = driver.undo.replay(&replay) else {
        panic!("replay should fail");
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    assert_eq!(repo.observe(), before);
    assert_eq!(repo.git(&["reflog", "show", "--format=%H", "HEAD"]), reflog);
}

/// If the ref transaction fails after HEAD moved, HEAD is put back.
#[test]
fn a_failed_replay_puts_head_back() {
    let repo = seeded();
    let driver = Driver::open(&repo);
    let before = repo.observe();
    let replay = invoke_core::UndoReplay {
        op: OpId(61),
        refs: vec![invoke_core::RestoreRef {
            name: BranchName("topic".into()).to_ref(),
            expect: None,
            // An object that is not in the repo: update-ref refuses it.
            restore: Some(oid(&"1".repeat(40))),
        }],
        tree: oid(&repo.index_tree()),
        index_effect: invoke_core::IndexEffect::Untouched,
        head: Some(invoke_core::HeadRestore::Branch(BranchName("old".into()))),
        expect: precondition::current(&repo.paths).unwrap(),
        stash: None,
    };
    let Event::OpFinished(_, OpOutcome::Failed(failure)) = driver.undo.replay(&replay) else {
        panic!("replay should fail");
    };
    assert!(
        failure.details.contains("HEAD put back"),
        "{}",
        failure.details
    );
    assert_eq!(repo.observe(), before);
}

// ---------------------------------------------------- refs snapshot read

#[test]
fn upstream_ahead_behind_and_gone_are_read() {
    let repo = TempRepo::new();
    repo.commit_file("a.txt", "1\n", "c1");
    let remote = repo.root.join("remote.git");
    let other = repo.root.join("other");
    let remote_arg = remote.to_str().unwrap().to_owned();
    repo.git(&["init", "-q", "--bare", "-b", "main", &remote_arg]);
    repo.git(&["remote", "add", "origin", &remote_arg]);
    repo.git(&["push", "-q", "-u", "origin", "main"]);
    repo.git(&["branch", "lonely"]);
    repo.git(&["branch", "doomed"]);
    repo.git(&["push", "-q", "-u", "origin", "doomed"]);

    // Two commits only here ...
    repo.commit_file("a.txt", "2\n", "c2");
    repo.commit_file("a.txt", "3\n", "c3");
    // ... and one only on the remote.
    repo.git(&["clone", "-q", &remote_arg, other.to_str().unwrap()]);
    let there = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&other)
            .envs(repo.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}");
    };
    fs::write(other.join("o.txt"), "o\n").unwrap();
    there(&["add", "o.txt"]);
    there(&["commit", "-q", "-m", "remote"]);
    there(&["push", "-q", "origin", "main"]);
    there(&["push", "-q", "origin", ":doomed"]);
    repo.git(&["fetch", "-q", "--prune", "origin"]);

    let disk = shell::read_state(&repo.paths).unwrap();
    let ups = &disk.refs.upstreams;
    let main = &ups[&BranchName("main".into())];
    assert_eq!(main.name.0, "refs/remotes/origin/main");
    assert_eq!(
        main.tracking,
        Tracking::Apart {
            ahead: 2,
            behind: 1
        }
    );
    assert_eq!(
        ups[&BranchName("doomed".into())].tracking,
        Tracking::Gone,
        "upstream deleted on the remote and pruned"
    );
    assert!(
        !ups.contains_key(&BranchName("lonely".into())),
        "no upstream, no entry"
    );
    assert_eq!(ups.len(), 2);
}

#[test]
fn a_branch_equal_to_its_upstream_is_zero_apart() {
    let repo = TempRepo::new();
    repo.commit_file("a.txt", "1\n", "c1");
    let remote = repo.root.join("remote.git");
    let remote_arg = remote.to_str().unwrap().to_owned();
    repo.git(&["init", "-q", "--bare", "-b", "main", &remote_arg]);
    repo.git(&["remote", "add", "origin", &remote_arg]);
    repo.git(&["push", "-q", "-u", "origin", "main"]);
    let disk = shell::read_state(&repo.paths).unwrap();
    assert_eq!(
        disk.refs.upstreams[&BranchName("main".into())].tracking,
        Tracking::Apart {
            ahead: 0,
            behind: 0
        }
    );
}

// ----------------------------------------------------------- name rules

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// The core's name rules agree with `git check-ref-format --branch`:
    /// anything the core accepts, git accepts; anything git accepts is
    /// accepted unless the core is stricter on purpose (`HEAD`, a leading
    /// dash, or the `@{...}` forms `--branch` expands).
    #[test]
    fn name_rules_agree_with_git(text in "[a-c./@{}~^:?*\\[ \\-]{1,6}(\\.lock)?") {
        let repo = SHARED.with(|repo| repo.root.clone());
        let output = Command::new("git")
            .args(["check-ref-format", "--branch", &text])
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        let git_accepts = output.status.success();
        let core_accepts = BranchName::parse(&text).is_ok();
        if core_accepts {
            prop_assert!(git_accepts, "core accepts {text:?}, git does not");
        }
        // `--branch` also expands `@` to HEAD and `@{-1}` to the last branch.
        let stricter = text == "HEAD" || text == "@" || text.starts_with('-') || text.contains("@{");
        if git_accepts && !stricter {
            prop_assert!(core_accepts, "git accepts {text:?}, core does not");
        }
    }
}

thread_local! {
    static SHARED: TempRepo = TempRepo::named("names");
}
