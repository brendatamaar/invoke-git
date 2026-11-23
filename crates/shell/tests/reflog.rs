//! P7: Reflog reading via `gix` and recovery actions (create branch at OID,
//! soft/mixed reset with expected-ref guard & SQLite-synced undo) against real
//! generated git repositories with `git reflog` as the oracle.
//!
//! System and global git config are shut out so the user's setup never leaks in.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use invoke_core::{
    BranchName, Command as Cmd, ErrorClass, Event, Notice, Oid, OpOutcome, ReflogEntry,
    ReflogTarget, RepoId, RepoState, ResetMode, open, step,
};
use shell::precondition::RepoPaths;
use shell::reflog::{ReflogEvent, list_reflog_targets, read_reflog, read_reflog_async};
use shell::store::Store;
use shell::undo::{Snapshotted, reconcile};
use shell::{Git, GitRunner, UndoRunner, discover, read::read_event};

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
            "invoke-git-reflog-{}-{}-{nanos}",
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
            ("GIT_AUTHOR_NAME", "Reflog Tester".into()),
            ("GIT_AUTHOR_EMAIL", "tester@example.com".into()),
            ("GIT_COMMITTER_NAME", "Reflog Tester".into()),
            ("GIT_COMMITTER_EMAIL", "tester@example.com".into()),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v))
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

    fn try_git_in(&self, dir: &Path, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }

    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        self.try_git_in(dir, args)
            .unwrap_or_else(|| panic!("git {args:?} in {} failed", dir.display()))
    }

    fn try_git(&self, args: &[&str]) -> Option<String> {
        self.try_git_in(&self.paths.workdir, args)
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.paths.workdir, args)
    }

    fn write(&self, path: &str, text: &str) {
        fs::write(self.paths.workdir.join(path), text).unwrap();
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.paths.workdir.join(path)).unwrap()
    }

    fn commit(&self, file: &str, content: &str, message: &str) -> Oid {
        self.write(file, content);
        self.git(&["add", file]);
        self.git(&["commit", "-q", "-m", message]);
        Oid::from_hex(&self.git(&["rev-parse", "HEAD"])).unwrap()
    }

    fn observe(&self) -> Observed {
        let refs = self
            .git(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .lines()
            .filter(|line| !line.starts_with("refs/invoke-git/"))
            .map(str::to_owned)
            .collect();
        Observed {
            head_sym: self.try_git(&["symbolic-ref", "-q", "HEAD"]),
            head_oid: self.try_git(&["rev-parse", "-q", "--verify", "HEAD"]),
            refs,
            index_entries: self.git(&["ls-files", "-s"]),
            index_tree: self.index_tree(),
        }
    }

    fn index_tree(&self) -> String {
        let copy = self.root.join("index-copy");
        let _ = fs::copy(self.paths.git_dir.join("index"), &copy);
        let output = Command::new("git")
            .arg("write-tree")
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .env("GIT_INDEX_FILE", &copy)
            .output()
            .unwrap();
        let _ = fs::remove_file(&copy);
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    head_sym: Option<String>,
    head_oid: Option<String>,
    refs: BTreeSet<String>,
    index_entries: String,
    index_tree: String,
}

#[derive(Debug, PartialEq, Eq)]
struct OracleRow {
    new_oid: String,
    selector: String,
    message: String,
    actor: String,
    email: String,
    seconds: i64,
    offset: i32,
}

fn parse_raw_offset(raw: &str) -> i32 {
    let sign = if raw.starts_with('-') { -1 } else { 1 };
    let digits = raw.trim_start_matches(['+', '-']);
    let hours: i32 = digits[0..2].parse().unwrap();
    let mins: i32 = digits[2..4].parse().unwrap();
    sign * (hours * 3600 + mins * 60)
}

fn git_reflog_oracle(repo: &TempRepo, dir: &Path, ref_spec: &str) -> Vec<OracleRow> {
    let out = repo.git_in(
        dir,
        &[
            "reflog",
            "show",
            ref_spec,
            "--format=%H%x1f%gd%x1f%gs%x1f%gn%x1f%ge",
        ],
    );
    if out.is_empty() {
        return Vec::new();
    }
    let dates = repo.git_in(
        dir,
        &["reflog", "show", ref_spec, "--date=raw", "--format=%gd"],
    );
    out.lines()
        .zip(dates.lines())
        .map(|(line, date_selector)| {
            let parts: Vec<&str> = line.split('\x1f').collect();
            assert_eq!(parts.len(), 5, "unexpected oracle line: {line:?}");
            // `HEAD@{1700000000 +0700}` -> `1700000000 +0700`
            let inside = date_selector
                .split_once("@{")
                .and_then(|(_, rest)| rest.strip_suffix('}'))
                .unwrap_or_else(|| panic!("unexpected raw reflog date: {date_selector:?}"));
            let (secs, off) = inside.split_once(' ').unwrap();
            OracleRow {
                new_oid: parts[0].to_owned(),
                selector: parts[1].to_owned(),
                message: parts[2].to_owned(),
                actor: parts[3].to_owned(),
                email: parts[4].to_owned(),
                seconds: secs.parse().unwrap(),
                offset: parse_raw_offset(off),
            }
        })
        .collect()
}

fn assert_matches_oracle(entries: &[ReflogEntry], oracle: &[OracleRow]) {
    assert_eq!(entries.len(), oracle.len(), "entry count mismatch");
    for (idx, (got, exp)) in entries.iter().zip(oracle.iter()).enumerate() {
        assert_eq!(got.index, idx);
        assert_eq!(got.new_oid.to_string(), exp.new_oid, "new_oid at {idx}");
        assert_eq!(got.selector, exp.selector, "selector at {idx}");
        assert_eq!(got.message, exp.message, "message at {idx}");
        assert_eq!(got.actor, exp.actor, "actor at {idx}");
        assert_eq!(got.email, exp.email, "email at {idx}");
        assert_eq!(got.seconds, exp.seconds, "seconds at {idx}");
        assert_eq!(got.offset, exp.offset, "offset at {idx}");
    }
    if let Some(oldest) = entries.last() {
        assert_eq!(
            oldest.old_oid, None,
            "initial creation entry has null old_oid"
        );
    }
    for pair in entries.windows(2) {
        let newer = &pair[0];
        let older = &pair[1];
        assert_eq!(
            newer.old_oid.as_ref(),
            Some(&older.new_oid),
            "old_oid chain broken between {} and {}",
            newer.selector,
            older.selector
        );
    }
}

struct Driver<'a> {
    repo: &'a TempRepo,
    state: RepoState,
    git: GitRunner,
    undo: UndoRunner,
    events: (mpsc::Sender<Event>, mpsc::Receiver<Event>),
}

impl<'a> Driver<'a> {
    fn new(repo: &'a TempRepo) -> Self {
        let git_bin = Git::detect("git").expect("git >= 2.40 required");
        let (state, commands) = open(RepoId {
            root_commit: None,
            remote_url: None,
        });
        let mut driver = Self {
            repo,
            state,
            git: GitRunner::new(git_bin.clone(), repo.paths.clone()).with_env(repo.env.clone()),
            undo: UndoRunner::new(git_bin, repo.paths.clone()).with_env(repo.env.clone()),
            events: mpsc::channel(),
        };
        driver.run_commands(commands);
        driver
    }

    fn send(&mut self, event: Event) {
        let (state, commands) = step(self.state.clone(), event);
        self.state = state;
        self.run_commands(commands);
    }

    fn run_commands(&mut self, commands: Vec<Cmd>) {
        let mut queue = commands;
        while !queue.is_empty() {
            let mut next = Vec::new();
            for command in queue {
                match command {
                    Cmd::ReadState { read, .. } => {
                        let event = read_event(&self.repo.paths, read);
                        let (state, cmds) = step(self.state.clone(), event);
                        self.state = state;
                        next.extend(cmds);
                    }
                    Cmd::SnapshotForUndo(point) => {
                        let event = match self.undo.snapshot(&point) {
                            Snapshotted::Taken(ev) | Snapshotted::Failed(ev) => ev,
                        };
                        let (state, cmds) = step(self.state.clone(), event);
                        self.state = state;
                        next.extend(cmds);
                    }
                    Cmd::RunGit { op, args, expect } => {
                        self.git.run(op, args, expect, self.events.0.clone());
                        let event = self
                            .events
                            .1
                            .recv_timeout(Duration::from_secs(10))
                            .expect("git op finished");
                        let (state, cmds) = step(self.state.clone(), event);
                        self.state = state;
                        next.extend(cmds);
                    }
                    Cmd::ReplayUndo(replay) => {
                        let event = self.undo.replay(&replay);
                        let (state, cmds) = step(self.state.clone(), event);
                        self.state = state;
                        next.extend(cmds);
                    }
                    Cmd::PruneUndo(ops) => {
                        let _ = self.undo.prune(&ops);
                    }
                    other => panic!("unexpected command in reflog test: {other:?}"),
                }
            }
            queue = next;
        }
    }

    fn refresh(&mut self) {
        self.send(Event::RefreshRequested);
    }
}

#[test]
fn head_and_branch_reflogs_match_git_reflog_oracle() {
    let repo = TempRepo::new();
    repo.commit("a.txt", "1\n", "first commit");
    repo.commit("a.txt", "2\n", "second commit");
    repo.git(&["switch", "-q", "-c", "feature"]);
    repo.commit("b.txt", "feat\n", "feature work");
    repo.git(&["switch", "-q", "main"]);
    repo.write("a.txt", "2 amended\n");
    repo.git(&["add", "a.txt"]);
    repo.git(&["commit", "-q", "--amend", "-m", "second commit (amended)"]);

    let head_entries = read_reflog(&repo.paths, &ReflogTarget::Head, None).unwrap();
    let head_oracle = git_reflog_oracle(&repo, &repo.paths.workdir, "HEAD");
    assert_matches_oracle(&head_entries, &head_oracle);
    assert!(head_entries.len() >= 5);

    let main_target = ReflogTarget::Branch(BranchName::parse("main").unwrap());
    let main_entries = read_reflog(&repo.paths, &main_target, None).unwrap();
    let main_oracle = git_reflog_oracle(&repo, &repo.paths.workdir, "main");
    assert_matches_oracle(&main_entries, &main_oracle);

    let feat_target = ReflogTarget::Branch(BranchName::parse("feature").unwrap());
    let feat_entries = read_reflog(&repo.paths, &feat_target, None).unwrap();
    let feat_oracle = git_reflog_oracle(&repo, &repo.paths.workdir, "feature");
    assert_matches_oracle(&feat_entries, &feat_oracle);

    // Limit caps newest entries and keeps exact 0-based selectors.
    let limited = read_reflog(&repo.paths, &ReflogTarget::Head, Some(2)).unwrap();
    assert_eq!(limited.len(), 2);
    assert_eq!(limited[0], head_entries[0]);
    assert_eq!(limited[1], head_entries[1]);

    // Target list starts with HEAD followed by sorted local branches.
    let targets = list_reflog_targets(&repo.paths).unwrap();
    assert_eq!(targets, vec![ReflogTarget::Head, feat_target, main_target]);
}

#[test]
fn unborn_missing_branch_and_disabled_reflog_return_empty() {
    // Unborn repository (0 commits, no .git/logs/HEAD).
    let unborn = TempRepo::new();
    assert_eq!(
        read_reflog(&unborn.paths, &ReflogTarget::Head, None).unwrap(),
        Vec::new()
    );
    assert_eq!(
        read_reflog(
            &unborn.paths,
            &ReflogTarget::Branch(BranchName::parse("main").unwrap()),
            None
        )
        .unwrap(),
        Vec::new()
    );
    assert_eq!(
        list_reflog_targets(&unborn.paths).unwrap(),
        vec![ReflogTarget::Head]
    );

    // Missing branch in a repo with commits.
    unborn.commit("a.txt", "1\n", "init");
    assert_eq!(
        read_reflog(
            &unborn.paths,
            &ReflogTarget::Branch(BranchName::parse("does-not-exist").unwrap()),
            None
        )
        .unwrap(),
        Vec::new()
    );

    // Repository with reflogs disabled (`core.logAllRefUpdates = false`).
    let no_log = TempRepo::new();
    no_log.git(&["config", "core.logAllRefUpdates", "false"]);
    let logs_dir = no_log.paths.git_dir.join("logs");
    if logs_dir.exists() {
        fs::remove_dir_all(&logs_dir).unwrap();
    }
    no_log.commit("a.txt", "1\n", "no reflog commit");
    assert_eq!(
        read_reflog(&no_log.paths, &ReflogTarget::Head, None).unwrap(),
        Vec::new()
    );
    assert_eq!(
        read_reflog(
            &no_log.paths,
            &ReflogTarget::Branch(BranchName::parse("main").unwrap()),
            None
        )
        .unwrap(),
        Vec::new()
    );
}

#[test]
fn linked_worktree_reads_worktree_head_and_common_branch_reflogs() {
    let repo = TempRepo::new();
    repo.commit("a.txt", "1\n", "base");
    let wt_dir = repo.root.join("linked-wt");
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "wt-branch",
        wt_dir.to_str().unwrap(),
    ]);
    fs::write(wt_dir.join("wt.txt"), "from worktree\n").unwrap();
    repo.git_in(&wt_dir, &["add", "wt.txt"]);
    repo.git_in(&wt_dir, &["commit", "-q", "-m", "worktree commit"]);

    let discovered = discover(&wt_dir).expect("discover linked worktree");
    let wt_paths = discovered.paths;

    let wt_head = read_reflog(&wt_paths, &ReflogTarget::Head, None).unwrap();
    let wt_head_oracle = git_reflog_oracle(&repo, &wt_dir, "HEAD");
    assert_matches_oracle(&wt_head, &wt_head_oracle);

    let main_target = ReflogTarget::Branch(BranchName::parse("main").unwrap());
    let main_from_wt = read_reflog(&wt_paths, &main_target, None).unwrap();
    let main_oracle = git_reflog_oracle(&repo, &wt_dir, "main");
    assert_matches_oracle(&main_from_wt, &main_oracle);
}

#[test]
fn async_reader_loads_entries_and_respects_cancel() {
    let repo = TempRepo::new();
    repo.commit("a.txt", "1\n", "one");
    repo.commit("a.txt", "2\n", "two");
    let expected = read_reflog(&repo.paths, &ReflogTarget::Head, None).unwrap();

    let rx = read_reflog_async(
        repo.paths.clone(),
        ReflogTarget::Head,
        None,
        Arc::new(AtomicBool::new(false)),
    );
    let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        event,
        ReflogEvent::Loaded {
            target: ReflogTarget::Head,
            entries: expected,
        }
    );

    let cancelled = Arc::new(AtomicBool::new(true));
    let rx_cancelled = read_reflog_async(repo.paths.clone(), ReflogTarget::Head, None, cancelled);
    assert!(rx_cancelled.recv_timeout(Duration::from_secs(5)).is_err());
}

#[test]
fn recover_dangling_commit_via_branch_and_undo_obeys_laws_8_and_9() {
    let repo = TempRepo::new();
    let c1 = repo.commit("a.txt", "1\n", "c1");
    repo.git(&["branch", "other"]);
    let _c2 = repo.commit("a.txt", "2\n", "c2");
    let c3 = repo.commit("a.txt", "3\n", "c3 lost later");
    // Drop c2 & c3 from `main`; c3 is now reachable only via reflog.
    repo.git(&["reset", "--hard", c1.as_hex()]);
    assert_eq!(repo.git(&["rev-parse", "main"]), c1.to_string());

    let entries = read_reflog(&repo.paths, &ReflogTarget::Head, None).unwrap();
    let lost_entry = entries
        .iter()
        .find(|e| e.new_oid == c3)
        .expect("c3 present in HEAD reflog")
        .clone();

    let mut driver = Driver::new(&repo);

    // 1. Create recovery branch without switching, then undo.
    let before = repo.observe();
    driver.send(lost_entry.create_branch_event("recover-stay", false));
    assert_eq!(repo.git(&["rev-parse", "recover-stay"]), c3.to_string());
    assert_eq!(repo.git(&["rev-parse", "other"]), c1.to_string());
    driver.send(Event::UndoRequested);
    assert_eq!(
        repo.observe(),
        before,
        "law 8: exact restore after branch create"
    );

    // 2. Create recovery branch and switch to it (checks out c3's files), then undo.
    let before_switch = repo.observe();
    driver.send(lost_entry.create_branch_event("recover-switch", true));
    assert_eq!(
        repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]),
        "recover-switch"
    );
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), c3.to_string());
    assert_eq!(repo.read("a.txt"), "3\n");
    assert_eq!(repo.git(&["rev-parse", "other"]), c1.to_string());
    driver.send(Event::UndoRequested);
    assert_eq!(
        repo.observe(),
        before_switch,
        "law 8: exact restore after branch create+switch"
    );
    assert_eq!(repo.read("a.txt"), "1\n");
}

#[test]
fn recover_via_soft_and_mixed_reset_on_branch_and_detached_head_with_undo() {
    let repo = TempRepo::new();
    let c1 = repo.commit("a.txt", "1\n", "c1");
    repo.git(&["branch", "other"]);
    let c2 = repo.commit("a.txt", "2\n", "c2");

    // Keep both an unstaged worktree edit and a staged edit to prove Soft/Mixed
    // never touch working-tree files and Mixed undo restores exact staged index.
    repo.write("staged.txt", "staged content\n");
    repo.git(&["add", "staged.txt"]);
    repo.write("work.txt", "unstaged worktree edit\n");

    let entries = read_reflog(&repo.paths, &ReflogTarget::Head, None).unwrap();
    let c1_entry = entries
        .iter()
        .find(|e| e.new_oid == c1)
        .expect("c1 present in HEAD reflog")
        .clone();

    let mut driver = Driver::new(&repo);

    // Case A: Soft reset on `main`, then undo.
    let before_soft = repo.observe();
    driver.send(c1_entry.reset_event(ResetMode::Soft));
    assert_eq!(repo.git(&["rev-parse", "main"]), c1.to_string());
    assert_eq!(repo.git(&["rev-parse", "other"]), c1.to_string());
    assert_eq!(repo.read("work.txt"), "unstaged worktree edit\n");
    assert_eq!(
        repo.index_tree(),
        before_soft.index_tree,
        "soft reset leaves index untouched"
    );
    driver.send(Event::UndoRequested);
    assert_eq!(repo.observe(), before_soft, "law 8: soft reset undo exact");
    assert_eq!(repo.read("work.txt"), "unstaged worktree edit\n");

    // Case B: Mixed reset on `main`, then undo.
    let before_mixed = repo.observe();
    driver.send(c1_entry.reset_event(ResetMode::Mixed));
    assert_eq!(repo.git(&["rev-parse", "main"]), c1.to_string());
    assert_eq!(repo.git(&["rev-parse", "other"]), c1.to_string());
    assert_eq!(repo.read("work.txt"), "unstaged worktree edit\n");
    assert_ne!(
        repo.index_tree(),
        before_mixed.index_tree,
        "mixed reset rewrites index to target tree"
    );
    driver.send(Event::UndoRequested);
    assert_eq!(
        repo.observe(),
        before_mixed,
        "law 8: mixed reset undo restores exact ref and staged index"
    );
    assert_eq!(repo.read("work.txt"), "unstaged worktree edit\n");

    // Case C: Detached HEAD reset (Mixed) moves only HEAD, leaves `main` & `other` untouched.
    repo.git(&["checkout", "-q", "--detach", c2.as_hex()]);
    driver.refresh();
    let before_detached = repo.observe();
    driver.send(c1_entry.reset_event(ResetMode::Mixed));
    assert_eq!(repo.git(&["rev-parse", "HEAD"]), c1.to_string());
    assert_eq!(repo.git(&["rev-parse", "main"]), c2.to_string());
    assert_eq!(repo.git(&["rev-parse", "other"]), c1.to_string());
    driver.send(Event::UndoRequested);
    assert_eq!(
        repo.observe(),
        before_detached,
        "law 8 & 9: detached reset undo exact"
    );
}

#[test]
fn sqlite_undo_stack_survives_restart_after_reflog_recovery() {
    let repo = TempRepo::new();
    let c1 = repo.commit("a.txt", "1\n", "c1");
    let c2 = repo.commit("a.txt", "2\n", "c2");
    repo.write("staged.txt", "keep me\n");
    repo.git(&["add", "staged.txt"]);

    let entries = read_reflog(&repo.paths, &ReflogTarget::Head, None).unwrap();
    let c1_entry = entries.iter().find(|e| e.new_oid == c1).unwrap().clone();

    let mut driver1 = Driver::new(&repo);
    let original = repo.observe();

    // Perform two reflog recoveries in session 1: mixed reset, then branch create.
    driver1.send(c1_entry.reset_event(ResetMode::Mixed));
    let after_reset = repo.observe();
    driver1.send(c1_entry.create_branch_event("recover-c1", false));
    assert_eq!(repo.git(&["rev-parse", "recover-c1"]), c1.to_string());
    assert_eq!(driver1.state.undo.entries.len(), 2);

    // Persist undo stack to SQLite (`P1`), reload and reconcile against `.git`.
    let store = Store::open_in_memory().unwrap();
    let key = repo.paths.git_dir.to_string_lossy().into_owned();
    store
        .save_undo(&key, driver1.state.undo.entries.iter().cloned())
        .unwrap();
    let (loaded, skipped) = store.load_undo(&key).unwrap();
    assert_eq!(skipped, 0);
    assert_eq!(loaded.len(), 2);

    let reconciled = reconcile(&repo.paths, loaded).unwrap();
    assert!(reconciled.orphans.is_empty());
    assert_eq!(reconciled.kept.len(), 2);

    // Simulate app restart with fresh RepoState + restored undo stack.
    let mut driver2 = Driver::new(&repo);
    driver2.send(Event::UndoStackRestored(reconciled.kept));

    // First undo reverses recovery branch creation.
    driver2.send(Event::UndoRequested);
    assert_eq!(repo.observe(), after_reset);

    // Second undo reverses mixed reset back to c2 + exact staged index.
    driver2.send(Event::UndoRequested);
    assert_eq!(repo.observe(), original);
    assert_eq!(repo.git(&["rev-parse", "main"]), c2.to_string());
}

#[test]
fn reset_guards_against_external_ref_move_lock_and_missing_object() {
    let repo = TempRepo::new();
    let c1 = repo.commit("a.txt", "1\n", "c1");
    let c2 = repo.commit("a.txt", "2\n", "c2");
    let git_bin = Git::detect("git").unwrap();
    let runner = GitRunner::new(git_bin, repo.paths.clone()).with_env(repo.env.clone());

    // 1. Expected-ref guard refuses when `main` moved externally after snapshot.
    let stale_state = Driver::new(&repo).state;
    let c3 = repo.commit("a.txt", "3\n", "external commit");
    let (_, commands) = step(
        stale_state,
        Event::ResetRequested {
            target: c1.clone(),
            mode: ResetMode::Soft,
        },
    );
    let (op, args, expect) = commands
        .into_iter()
        .find_map(|c| match c {
            Cmd::RunGit { op, args, expect } => Some((op, args, expect)),
            _ => None,
        })
        .expect("core emitted RunGit");
    let (tx, rx) = mpsc::channel();
    runner.run(op, args, expect, tx);
    let Event::OpFinished(_, OpOutcome::Failed(failure)) =
        rx.recv_timeout(Duration::from_secs(5)).unwrap()
    else {
        panic!("expected PreconditionChanged failure");
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    assert_eq!(repo.git(&["rev-parse", "main"]), c3.to_string());

    // 2. Active `index.lock` refuses reset with PreconditionChanged.
    let fresh_state = Driver::new(&repo).state;
    let (_, commands) = step(
        fresh_state,
        Event::ResetRequested {
            target: c2.clone(),
            mode: ResetMode::Soft,
        },
    );
    let (op, args, expect) = commands
        .into_iter()
        .find_map(|c| match c {
            Cmd::RunGit { op, args, expect } => Some((op, args, expect)),
            _ => None,
        })
        .unwrap();
    let lock = repo.paths.git_dir.join("index.lock");
    fs::write(&lock, "").unwrap();
    let (tx, rx) = mpsc::channel();
    runner.run(op, args, expect, tx);
    let outcome = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let _ = fs::remove_file(&lock);
    let Event::OpFinished(_, OpOutcome::Failed(failure)) = outcome else {
        panic!("expected lock refusal");
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    assert_eq!(repo.git(&["rev-parse", "main"]), c3.to_string());

    // 3. Resetting to a pruned/missing OID classifies as Expected, not Bug.
    let mut driver = Driver::new(&repo);
    let missing = Oid::from_hex(&"f".repeat(40)).unwrap();
    driver.send(Event::ResetRequested {
        target: missing,
        mode: ResetMode::Soft,
    });
    match driver.state.notice {
        Some(Notice::OpEnded {
            outcome: OpOutcome::Failed(failure),
            ..
        }) => assert_eq!(failure.class, ErrorClass::Expected),
        other => panic!("expected OpEnded Expected failure, got {other:?}"),
    }
    assert_eq!(repo.git(&["rev-parse", "main"]), c3.to_string());
}
