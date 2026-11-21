//! Stash end to end (backlog task 20): the real core and the real shell on
//! generated temp repos, with real git as the oracle. System and global git
//! config are shut out, so the user's setup never leaks in. Covers law 8
//! (undo of a drop restores the stash list), law 9 (no branch moves) and
//! rule 5 (a stash that moved is refused, never replaced by another).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use invoke_core::{
    Action, ChangeSet, Command as Cmd, ErrorClass, Event, Expect, GitArgs, Irreversible, Notice,
    Oid, OpId, OpOutcome, Phase, Refusal, RepoId, RepoState, UndoMoved, open, step,
};
use shell::precondition::{self, RepoPaths};
use shell::undo::{Snapshotted, stash_keep_ref};
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
            "invoke-git-stash-{}-{}-{nanos}",
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

    fn exists(&self, path: &str) -> bool {
        self.paths.workdir.join(path).exists()
    }

    fn stage(&self, path: &str, text: &str) {
        self.write(path, text);
        self.git(&["add", path]);
    }

    /// Every ref except the stash and our own bookkeeping, and HEAD: what
    /// law 9 says a stash never moves.
    fn branches(&self) -> BTreeSet<String> {
        let mut refs: BTreeSet<String> = self
            .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .lines()
            .filter(|line| {
                !line.starts_with("refs/invoke-git/") && !line.starts_with("refs/stash ")
            })
            .map(str::to_owned)
            .collect();
        refs.insert(format!("HEAD {}", self.git(&["rev-parse", "HEAD"])));
        refs
    }

    /// The stash list as git reports it: commit id and message, newest first.
    fn stash_list(&self) -> Vec<(String, String)> {
        self.try_git(&["stash", "list", "--format=%H\t%gs"])
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let (id, message) = line.split_once('\t').unwrap();
                (id.to_owned(), message.to_owned())
            })
            .collect()
    }

    fn stash_id(&self, n: usize) -> Oid {
        Oid::from_hex(&self.git(&["rev-parse", &format!("stash@{{{n}}}")])).unwrap()
    }

    fn status(&self) -> String {
        self.git(&["status", "--porcelain", "--untracked-files=all"])
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

/// The real core driving the real shell, one command at a time.
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

    fn save(&mut self, message: &str, include_untracked: bool) {
        self.deliver(Event::StashSaveRequested {
            message: message.into(),
            include_untracked,
        });
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

    fn succeeded(&self) -> bool {
        self.last_outcome() == Some(OpOutcome::Succeeded)
    }

    fn refusal(&self) -> Option<(Action, Refusal)> {
        match &self.state.notice {
            Some(Notice::Refused { action, reason }) => Some((*action, reason.clone())),
            _ => None,
        }
    }

    /// The stash list the core holds, as (id, message).
    fn core_stashes(&self) -> Vec<(String, String)> {
        self.state
            .fresh_disk()
            .expect("fresh snapshot")
            .stashes
            .iter()
            .map(|entry| (entry.id.to_string(), entry.message.clone()))
            .collect()
    }
}

fn seeded() -> TempRepo {
    let repo = TempRepo::new();
    repo.stage("a.txt", "one\n");
    repo.git(&["commit", "-q", "-m", "first"]);
    repo.git(&["branch", "dev"]);
    repo.git(&["branch", "topic"]);
    repo.git(&["tag", "v1"]);
    repo
}

/// Staged, unstaged and untracked changes, the way a messy worktree looks.
fn make_mess(repo: &TempRepo) {
    repo.write("a.txt", "two\n");
    repo.stage("b.txt", "new\n");
    repo.write("c.txt", "untracked\n");
}

/// Save with untracked files: the worktree is clean afterwards, the
/// untracked file is inside the stash commit, the core's list (read through
/// gix) matches git's, and no branch moved (law 9).
#[test]
fn save_includes_untracked_files_and_moves_no_branch() {
    let repo = seeded();
    make_mess(&repo);
    let branches = repo.branches();
    let mut driver = Driver::open(&repo);

    driver.save("wip: mess", true);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.status(), "");
    assert!(
        !repo.exists("c.txt"),
        "the untracked file went into the stash"
    );
    assert_eq!(repo.read("a.txt"), "one\n");
    let list = repo.stash_list();
    assert_eq!(list.len(), 1);
    assert!(list[0].1.contains("wip: mess"), "{list:?}");
    assert!(
        repo.git(&["ls-tree", "-r", "--name-only", "stash@{0}^3"])
            .contains("c.txt")
    );
    assert_eq!(repo.branches(), branches);
    // The core reads the same list through gix.
    assert_eq!(driver.core_stashes(), list);
}

/// Without the flag, untracked files stay where they are.
#[test]
fn save_without_untracked_leaves_them_in_place() {
    let repo = seeded();
    make_mess(&repo);
    let mut driver = Driver::open(&repo);

    driver.save("", false);
    assert!(driver.succeeded());
    assert!(repo.exists("c.txt"));
    assert_eq!(repo.status(), "?? c.txt");
    assert_eq!(repo.stash_list().len(), 1);
}

/// Nothing to stash is an expected outcome, not a success and not a bug.
#[test]
fn saving_nothing_is_an_expected_failure() {
    let repo = seeded();
    repo.write("c.txt", "untracked only\n");
    let mut driver = Driver::open(&repo);

    driver.save("", false);
    match driver.last_outcome() {
        Some(OpOutcome::Failed(failure)) => assert_eq!(failure.class, ErrorClass::Expected),
        other => panic!("{other:?}"),
    }
    assert!(repo.stash_list().is_empty());
    assert!(driver.state.undo.entries.is_empty());
    assert!(repo.saved_refs().is_empty());
}

/// Apply keeps the stash and brings every kind of change back.
#[test]
fn apply_restores_changes_and_keeps_the_stash() {
    let repo = seeded();
    make_mess(&repo);
    let mut driver = Driver::open(&repo);
    driver.save("wip", true);
    let list = repo.stash_list();
    let branches = repo.branches();
    let id = repo.stash_id(0);

    driver.deliver(Event::StashApplyRequested(id));
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.read("a.txt"), "two\n");
    assert_eq!(repo.read("b.txt"), "new\n");
    assert_eq!(repo.read("c.txt"), "untracked\n");
    assert_eq!(repo.stash_list(), list, "apply keeps the stash");
    assert_eq!(driver.core_stashes(), list);
    assert_eq!(repo.branches(), branches);
}

/// Pop brings the changes back and removes exactly that stash.
#[test]
fn pop_restores_changes_and_removes_the_stash() {
    let repo = seeded();
    make_mess(&repo);
    let mut driver = Driver::open(&repo);
    driver.save("wip", true);
    let branches = repo.branches();
    let id = repo.stash_id(0);

    driver.deliver(Event::StashPopRequested(id));
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.read("a.txt"), "two\n");
    assert_eq!(repo.read("c.txt"), "untracked\n");
    assert!(repo.stash_list().is_empty());
    assert!(driver.core_stashes().is_empty());
    assert_eq!(repo.branches(), branches);
}

/// Stashes are addressed by id: after another stash lands on top, popping
/// the older one pops that one, not whatever sits at `stash@{0}` now.
#[test]
fn pop_uses_the_id_not_the_position() {
    let repo = seeded();
    repo.write("a.txt", "first stash\n");
    let mut driver = Driver::open(&repo);
    driver.save("older", false);
    let older = repo.stash_id(0);
    repo.write("a.txt", "second stash\n");
    repo.git(&["stash", "push", "-q", "-m", "newer"]);
    driver.outside_change();
    assert_eq!(repo.stash_list().len(), 2);

    driver.deliver(Event::StashPopRequested(older));
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.read("a.txt"), "first stash\n");
    let left = repo.stash_list();
    assert_eq!(left.len(), 1);
    assert!(left[0].1.contains("newer"), "{left:?}");
}

/// A pop that conflicts: an expected outcome. The stash stays (git's own
/// behaviour), the repo is in git's own conflicted state, no undo point is
/// kept, and no branch moved.
#[test]
fn a_conflicting_pop_keeps_the_stash_and_leaves_git_in_its_own_state() {
    let repo = seeded();
    repo.write("a.txt", "from the stash\n");
    let mut driver = Driver::open(&repo);
    driver.save("conflicting", false);
    repo.stage("a.txt", "from a commit\n");
    repo.git(&["commit", "-q", "-m", "diverge"]);
    driver.outside_change();
    let branches = repo.branches();
    let list = repo.stash_list();
    let id = repo.stash_id(0);
    let undo_entries = driver.state.undo.entries.len();

    driver.deliver(Event::StashPopRequested(id));
    match driver.last_outcome() {
        Some(OpOutcome::Failed(failure)) => {
            assert_eq!(failure.class, ErrorClass::Expected, "{}", failure.details);
            assert!(failure.details.contains("CONFLICT"), "{}", failure.details);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(repo.stash_list(), list, "the stash is kept");
    assert_eq!(driver.core_stashes(), list, "and the core sees it");
    assert!(!repo.git(&["ls-files", "-u"]).is_empty(), "unmerged paths");
    assert!(repo.read("a.txt").contains("<<<<<<<"));
    assert_eq!(repo.branches(), branches);
    assert_eq!(driver.state.undo.entries.len(), undo_entries);
    assert!(
        repo.saved_refs()
            .iter()
            .all(|name| !name.ends_with("-stash")),
        "no saved stash commit is left behind: {:?}",
        repo.saved_refs()
    );
    assert!(matches!(
        driver.state.fresh_disk().unwrap().phase,
        Phase::Clean | Phase::Merging { .. }
    ));
}

/// Apply over local changes git would overwrite: refused by git as an
/// expected outcome, nothing changes.
#[test]
fn apply_over_conflicting_local_changes_changes_nothing() {
    let repo = seeded();
    repo.write("a.txt", "stashed\n");
    let mut driver = Driver::open(&repo);
    driver.save("s", false);
    repo.write("a.txt", "local edit\n");
    driver.outside_change();
    let list = repo.stash_list();
    let id = repo.stash_id(0);

    driver.deliver(Event::StashApplyRequested(id));
    match driver.last_outcome() {
        Some(OpOutcome::Failed(failure)) => {
            assert_eq!(failure.class, ErrorClass::Expected, "{}", failure.details);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(repo.read("a.txt"), "local edit\n");
    assert_eq!(repo.stash_list(), list);
}

/// Drop removes the stash, keeps its commit alive under our own ref, and
/// the undo puts it back on top with the same id and message (law 8).
#[test]
fn drop_is_undone_by_putting_the_stash_back() {
    let repo = seeded();
    make_mess(&repo);
    let mut driver = Driver::open(&repo);
    driver.save("keep me", true);
    let list = repo.stash_list();
    let branches = repo.branches();
    let id = repo.stash_id(0);

    driver.deliver(Event::StashDropRequested(id.clone()));
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert!(repo.stash_list().is_empty());
    let kept = stash_keep_ref(OpId(1));
    assert!(
        repo.saved_refs().contains(&kept),
        "the dropped commit is kept alive: {:?}",
        repo.saved_refs()
    );
    assert_eq!(repo.git(&["rev-parse", &kept]), id.to_string());
    assert_eq!(driver.state.undo.entries.len(), 2);

    driver.deliver(Event::UndoRequested);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.stash_list(), list);
    assert_eq!(driver.core_stashes(), list);
    assert_eq!(repo.branches(), branches);
    assert!(
        !repo.saved_refs().contains(&kept),
        "the keep-alive ref goes with the point"
    );
    // The work is whole: popping the restored stash brings the files back.
    driver.deliver(Event::StashPopRequested(id));
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(repo.read("c.txt"), "untracked\n");
}

/// Dropping an older stash cannot be undone exactly (it would land on top),
/// so the stack says so, and the commit is still kept for recovery.
#[test]
fn dropping_an_older_stash_is_not_undoable_but_not_lost() {
    let repo = seeded();
    repo.write("a.txt", "one stash\n");
    let mut driver = Driver::open(&repo);
    driver.save("older", false);
    let older = repo.stash_id(0);
    repo.write("a.txt", "two stash\n");
    driver.outside_change();
    driver.save("newer", false);
    let top = repo.stash_id(0);

    driver.deliver(Event::StashDropRequested(older.clone()));
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let left = repo.stash_list();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].0, top.to_string(), "the newer stash is untouched");
    let kept = repo
        .saved_refs()
        .into_iter()
        .find(|name| name.ends_with("-stash"))
        .expect("the dropped commit is kept");
    assert_eq!(repo.git(&["rev-parse", &kept]), older.to_string());

    driver.deliver(Event::UndoRequested);
    assert_eq!(
        driver.refusal(),
        Some((
            Action::Undo,
            Refusal::NotUndoable(Irreversible::StashNotOnTop)
        ))
    );
    assert_eq!(repo.stash_list(), left);
}

/// Undo of a drop refuses when the stash list moved since.
#[test]
fn undo_of_a_drop_is_refused_after_another_stash() {
    let repo = seeded();
    repo.write("a.txt", "one\n\nmore\n");
    let mut driver = Driver::open(&repo);
    driver.save("s", false);
    let id = repo.stash_id(0);
    driver.deliver(Event::StashDropRequested(id));
    assert!(driver.succeeded());
    repo.write("a.txt", "someone else\n");
    repo.git(&["stash", "push", "-q", "-m", "outside"]);
    driver.outside_change();
    let before = repo.stash_list();

    driver.deliver(Event::UndoRequested);
    assert_eq!(
        driver.refusal(),
        Some((
            Action::Undo,
            Refusal::Changed(UndoMoved::Ref(invoke_core::stash::stash_ref()))
        ))
    );
    assert_eq!(repo.stash_list(), before);
}

/// Save, apply and pop rewrite working files, which undo never does: an
/// undo stops there with the reason instead of reversing an older commit.
#[test]
fn undo_after_a_save_says_why_it_cannot() {
    let repo = seeded();
    make_mess(&repo);
    let mut driver = Driver::open(&repo);
    driver.save("s", true);
    assert!(driver.succeeded());

    driver.deliver(Event::UndoRequested);
    assert_eq!(
        driver.refusal(),
        Some((
            Action::Undo,
            Refusal::NotUndoable(Irreversible::StashRewroteFiles)
        ))
    );
    assert_eq!(repo.stash_list().len(), 1);
}

/// A stash that is gone (dropped by someone else) is refused by the core,
/// before anything is written.
#[test]
fn a_stash_dropped_outside_is_refused_by_the_core() {
    let repo = seeded();
    repo.write("a.txt", "x\n");
    let mut driver = Driver::open(&repo);
    driver.save("s", false);
    let id = repo.stash_id(0);
    repo.git(&["stash", "drop", "-q"]);
    driver.outside_change();
    let branches = repo.branches();

    for event in [
        Event::StashApplyRequested(id.clone()),
        Event::StashPopRequested(id.clone()),
        Event::StashDropRequested(id.clone()),
    ] {
        let action = match &event {
            Event::StashApplyRequested(_) => Action::StashApply,
            Event::StashPopRequested(_) => Action::StashPop,
            _ => Action::StashDrop,
        };
        driver.deliver(event);
        assert_eq!(
            driver.refusal(),
            Some((action, Refusal::StashGone(id.clone())))
        );
    }
    assert!(repo.stash_list().is_empty());
    assert_eq!(repo.read("a.txt"), "one\n");
    assert_eq!(repo.branches(), branches);
}

/// The race the core cannot see: another stash lands on top after the core
/// read the list and before the write runs. The shell finds a different
/// entry at the position and refuses; nothing is popped or dropped.
#[test]
fn a_stash_that_moves_under_the_write_is_refused_by_the_shell() {
    // A fresh scenario per request: after the refusal the core re-reads and
    // would plan the next request correctly.
    for which in 0..3 {
        let repo = seeded();
        repo.write("a.txt", "mine\n");
        let mut driver = Driver::open(&repo);
        driver.save("mine", false);
        let mine = repo.stash_id(0);
        // No outside_change(): the core's snapshot still says `mine` is at 0.
        repo.write("b.txt", "intruder\n");
        repo.git(&["add", "b.txt"]);
        repo.git(&["stash", "push", "-q", "-m", "intruder"]);
        let list = repo.stash_list();
        let branches = repo.branches();
        let worktree = repo.status();

        driver.deliver(match which {
            0 => Event::StashPopRequested(mine),
            1 => Event::StashDropRequested(mine),
            _ => Event::StashApplyRequested(mine),
        });
        match driver.last_outcome() {
            Some(OpOutcome::Failed(failure)) => {
                assert_eq!(failure.class, ErrorClass::PreconditionChanged);
                assert!(
                    failure.details.contains("stash moved"),
                    "{}",
                    failure.details
                );
            }
            other => panic!("{which}: {other:?}"),
        }
        assert_eq!(repo.stash_list(), list);
        assert_eq!(repo.status(), worktree);
        assert_eq!(repo.branches(), branches);
        // Only the save is on the stack: the refused write left no point.
        assert_eq!(driver.state.undo.entries.len(), 1);
        assert_eq!(driver.state.undo.pending, invoke_core::UndoPending::Idle);
    }
}

/// The runner on its own, with arguments built by hand: a wrong id at the
/// position is refused before git runs (rule 5).
#[test]
fn the_runner_refuses_a_wrong_stash_id() {
    let repo = seeded();
    repo.write("a.txt", "x\n");
    repo.git(&["stash", "push", "-q"]);
    let list = repo.stash_list();
    let runner =
        GitRunner::new(Git::detect("git").unwrap(), repo.paths.clone()).with_env(repo.env.clone());
    let expect: Expect = precondition::current(&repo.paths).unwrap();
    let wrong = Oid::from_hex(&"1".repeat(40)).unwrap();
    for args in [
        GitArgs::StashPop {
            index: 0,
            stash: wrong.clone(),
        },
        GitArgs::StashDrop {
            index: 0,
            stash: wrong.clone(),
        },
        GitArgs::StashApply {
            index: 5,
            stash: repo.stash_id(0),
        },
    ] {
        let (sender, events) = mpsc::channel();
        runner.run(OpId(9), args, expect.clone(), sender);
        let Event::OpFinished(_, OpOutcome::Failed(failure)) =
            events.recv_timeout(Duration::from_secs(30)).unwrap()
        else {
            panic!("expected a refusal");
        };
        assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    }
    assert_eq!(repo.stash_list(), list);
    assert_eq!(repo.read("a.txt"), "one\n");
}

/// The shell's own check on a replayed drop: the stash list must have the
/// expected newest entry, or nothing is stored. With the right one, the
/// entry comes back with its id and message.
#[test]
fn replaying_a_drop_checks_the_stash_list_first() {
    let repo = seeded();
    repo.write("a.txt", "x\n");
    repo.git(&["stash", "push", "-q", "-m", "work"]);
    let entry = invoke_core::StashEntry {
        id: repo.stash_id(0),
        message: repo.stash_list()[0].1.clone(),
    };
    repo.git(&["stash", "drop", "-q"]);
    let driver = Driver::open(&repo);
    let replay = |expect_top: Option<Oid>| invoke_core::UndoReplay {
        op: OpId(60),
        refs: Vec::new(),
        tree: entry.id.clone(),
        expect: precondition::current(&repo.paths).unwrap(),
        stash: Some(invoke_core::StashRestore {
            entry: entry.clone(),
            expect_top,
        }),
    };

    let wrong = Some(Oid::from_hex(&"2".repeat(40)).unwrap());
    let Event::OpFinished(_, OpOutcome::Failed(failure)) = driver.undo.replay(&replay(wrong))
    else {
        panic!("a stale replay must be refused");
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    assert!(repo.stash_list().is_empty());

    let done = driver.undo.replay(&replay(None));
    assert_eq!(done, Event::OpFinished(OpId(60), OpOutcome::Succeeded));
    assert_eq!(
        repo.stash_list(),
        vec![(entry.id.to_string(), entry.message.clone())]
    );
}
