//! Task 23 (core half): the conflict flow end to end. The real core's
//! `step()` drives the real shell (git runner, undo runner, state read) on
//! generated repos in temp folders, with system and global git config shut
//! out. Conflicts are made by plain git (or by the app's own pull) and
//! everything after that goes through the core's requests.
//!
//! The environment and the isolated global config set every editor to
//! `false`, so any git run that waits for an editor fails: continuing works
//! only because the runner sets `GIT_EDITOR=true` for it.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use invoke_core::{
    Action, BranchName, Command, ConflictKind, ConflictRefusal, Conflicts, ErrorClass, Event,
    FileState, Irreversible, Notice, Oid, OpOutcome, Operation, Phase, PullMode, Refusal,
    RemoteBranch, RepoId, RepoPath, RepoState, ResolveChoice, StageContent, open, step,
};
use shell::precondition::RepoPaths;
use shell::undo::Snapshotted;
use shell::{Git, GitRunner, UndoRunner, read::read_event, read_conflict_content};

/// What the core said about one request.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Succeeded,
    Refused(Refusal),
    Failed(ErrorClass, String),
    Nothing,
}

struct Fx {
    root: PathBuf,
    dir: PathBuf,
    env: Vec<(OsString, OsString)>,
}

/// Everything an abort must put back exactly.
#[derive(Debug, PartialEq, Eq)]
struct Picture {
    refs: String,
    head: String,
    index: String,
    status: String,
    files: Vec<(String, Vec<u8>)>,
}

impl Fx {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let root = std::env::temp_dir().join(format!(
            "invoke-git-conflicts-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let global = root.join("gitconfig");
        fs::write(
            &global,
            "[core]\n\teditor = false\n[sequence]\n\teditor = false\n[init]\n\tdefaultBranch = main\n",
        )
        .unwrap();
        let env = [
            ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
            ("GIT_CONFIG_GLOBAL", global.into_os_string()),
            // An editor that always fails: continuing must not run it.
            ("GIT_EDITOR", "false".into()),
            ("GIT_SEQUENCE_EDITOR", "false".into()),
            ("GIT_AUTHOR_NAME", "Test".into()),
            ("GIT_AUTHOR_EMAIL", "test@example.com".into()),
            ("GIT_COMMITTER_NAME", "Test".into()),
            ("GIT_COMMITTER_EMAIL", "test@example.com".into()),
        ]
        .into_iter()
        .map(|(key, value)| (OsString::from(key), value))
        .collect();
        let dir = root.join("repo");
        fs::create_dir_all(&dir).unwrap();
        let fx = Self { root, dir, env };
        fx.git(&["init", "-q", "-b", "main"]);
        fx
    }

    fn try_git_in(&self, dir: &Path, args: &[&str]) -> std::process::Output {
        Process::new("git")
            .args(args)
            .current_dir(dir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap()
    }

    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.try_git_in(dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.dir, args)
    }

    /// Runs git, which must stop (a conflict).
    fn git_stops(&self, args: &[&str]) {
        let output = self.try_git_in(&self.dir, args);
        assert!(!output.status.success(), "git {args:?} should have stopped");
    }

    fn write(&self, path: &str, text: &str) {
        self.write_bytes(path, text.as_bytes());
    }

    fn write_bytes(&self, path: &str, bytes: &[u8]) {
        let full = self.dir.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, bytes).unwrap();
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.dir.join(path)).unwrap()
    }

    fn commit(&self, path: &str, text: &str, message: &str) {
        self.write(path, text);
        self.git(&["add", "--", path]);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn paths(&self) -> RepoPaths {
        RepoPaths {
            workdir: self.dir.clone(),
            git_dir: self.dir.join(".git"),
        }
    }

    fn git_dir(&self) -> PathBuf {
        self.dir.join(".git")
    }

    fn app(&self) -> App {
        App::open(self)
    }

    /// Refs, `HEAD`, the index entries, the status and the files.
    fn picture(&self) -> Picture {
        let mut files = Vec::new();
        let mut stack = vec![self.dir.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.file_name().is_some_and(|name| name == ".git") {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let name = path
                        .strip_prefix(&self.dir)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    files.push((name, fs::read(&path).unwrap()));
                }
            }
        }
        files.sort();
        Picture {
            refs: self.git(&["for-each-ref", "--format=%(refname) %(objectname)"]),
            head: self.git(&["symbolic-ref", "-q", "--short", "HEAD"]),
            index: self.git(&["ls-files", "-s"]),
            status: self.git(&["status", "--porcelain"]),
            files,
        }
    }

    fn rev(&self, name: &str) -> String {
        self.git(&["rev-parse", name])
    }

    /// `main` and `topic` both change each of `files` from "base" to their
    /// own text, then `main` merges `topic` and stops. `side` is a branch
    /// nobody targets.
    fn conflicting_merge(&self, files: &[&str]) {
        for file in files {
            self.write(file, "base\n");
        }
        self.git(&["add", "."]);
        self.git(&["commit", "-q", "-m", "base"]);
        self.git(&["branch", "side"]);
        self.git(&["switch", "-q", "-c", "topic"]);
        for file in files {
            self.write(file, &format!("topic {file}\n"));
        }
        self.git(&["commit", "-q", "-am", "topic"]);
        self.git(&["switch", "-q", "main"]);
        for file in files {
            self.write(file, &format!("main {file}\n"));
        }
        self.git(&["commit", "-q", "-am", "main"]);
        self.git_stops(&["merge", "--no-edit", "topic"]);
    }

    /// `main` edits `a.txt` to "main"; `topic` (three commits) edits it to
    /// "t1", "t2", "t3". `topic` is checked out and `git rebase main` has
    /// stopped at step 1 of 3.
    fn conflicting_rebase(&self) {
        self.commit("a.txt", "0\n", "base");
        self.git(&["branch", "side"]);
        self.git(&["switch", "-q", "-c", "topic"]);
        for n in 1..=3 {
            self.commit("a.txt", &format!("t{n}\n"), &format!("t{n}"));
        }
        self.git(&["switch", "-q", "main"]);
        self.commit("a.txt", "main\n", "main");
        self.git(&["switch", "-q", "topic"]);
        self.git_stops(&["rebase", "main"]);
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// The real core driving the real shell.
struct App {
    state: RepoState,
    paths: RepoPaths,
    runner: GitRunner,
    undo: UndoRunner,
}

impl App {
    fn open(fx: &Fx) -> Self {
        let git = || Git::detect("git").expect("git on PATH");
        let (state, commands) = open(RepoId {
            root_commit: None,
            remote_url: None,
        });
        let mut app = Self {
            state,
            paths: fx.paths(),
            runner: GitRunner::new(git(), fx.paths()).with_env(fx.env.clone()),
            undo: UndoRunner::new(git(), fx.paths()).with_env(fx.env.clone()),
        };
        for command in commands {
            app.exec(command);
        }
        app
    }

    fn deliver(&mut self, event: Event) {
        let (state, commands) = step(self.state.clone(), event);
        self.state = state;
        for command in commands {
            self.exec(command);
        }
    }

    fn exec(&mut self, command: Command) {
        match command {
            Command::ReadState { read, .. } => self.deliver(read_event(&self.paths, read)),
            Command::SnapshotForUndo(point) => match self.undo.snapshot(&point) {
                Snapshotted::Taken(event) | Snapshotted::Failed(event) => self.deliver(event),
            },
            Command::RunGit { op, args, expect } => {
                let (sender, events) = mpsc::channel();
                self.runner.run(op, args, expect, sender);
                loop {
                    let event = events
                        .recv_timeout(Duration::from_secs(60))
                        .expect("git operation never finished");
                    let done = matches!(event, Event::OpFinished(..));
                    self.deliver(event);
                    if done {
                        break;
                    }
                }
            }
            Command::ReplayUndo(replay) => self.deliver(self.undo.replay(&replay)),
            Command::PruneUndo(ops) => self.undo.prune(&ops),
            other => panic!("not driven here: {other:?}"),
        }
    }

    /// Sends a request with a clean notice and reports how it ended.
    fn request(&mut self, event: Event) -> Outcome {
        self.deliver(Event::NoticeDismissed);
        self.deliver(event);
        match &self.state.notice {
            Some(Notice::Refused { reason, .. }) => Outcome::Refused(reason.clone()),
            Some(Notice::OpEnded { outcome, .. }) => match outcome {
                OpOutcome::Succeeded => Outcome::Succeeded,
                OpOutcome::Failed(failure) => {
                    Outcome::Failed(failure.class, failure.details.clone())
                }
                OpOutcome::Cancelled => panic!("cancelled"),
            },
            Some(other) => panic!("{other:?}"),
            None => Outcome::Nothing,
        }
    }

    fn refresh(&mut self) {
        self.deliver(Event::RefreshRequested);
    }

    fn phase(&self) -> &Phase {
        &self.state.fresh_disk().expect("a fresh snapshot").phase
    }

    fn conflicts(&self) -> &Conflicts {
        self.phase().conflicts().expect("a paused operation")
    }

    fn resolve(&mut self, path: &str, choice: ResolveChoice) -> Outcome {
        self.request(Event::ResolveFileRequested {
            path: RepoPath(path.into()),
            choice,
        })
    }

    fn continue_(&mut self) -> Outcome {
        self.request(Event::ContinueRequested)
    }

    fn unresolved(&self) -> Vec<String> {
        self.conflicts()
            .unresolved()
            .map(|(path, _)| path.0.clone())
            .collect()
    }

    fn resolved(&self) -> Vec<String> {
        self.conflicts()
            .files()
            .iter()
            .filter(|file| file.state == FileState::Resolved)
            .map(|file| file.path.0.clone())
            .collect()
    }
}

fn refused(reason: ConflictRefusal) -> Outcome {
    Outcome::Refused(Refusal::Conflict(reason))
}

// --- merge ---------------------------------------------------------------

#[test]
fn merge_resolves_ours_theirs_and_manual_then_continues() {
    let fx = Fx::new();
    fx.conflicting_merge(&["a.txt", "b.txt", "c.txt"]);
    let side_before = fx.rev("side");
    let mut app = fx.app();

    assert!(matches!(app.phase(), Phase::Merging { .. }));
    assert_eq!(app.unresolved(), ["a.txt", "b.txt", "c.txt"]);
    assert!(app.resolved().is_empty());

    // Continue is refused while any file is open, and nothing is written.
    let before = fx.picture();
    assert_eq!(
        app.continue_(),
        refused(ConflictRefusal::Unresolved { remaining: 3 })
    );
    assert_eq!(fx.picture(), before);
    assert!(fx.git_dir().join("MERGE_HEAD").exists());

    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Ours),
        Outcome::Succeeded
    );
    assert_eq!(fx.read("a.txt"), "main a.txt\n");
    assert_eq!(app.unresolved(), ["b.txt", "c.txt"]);
    assert_eq!(app.resolved(), ["a.txt"]);
    assert_eq!(
        app.continue_(),
        refused(ConflictRefusal::Unresolved { remaining: 2 })
    );

    assert_eq!(
        app.resolve("b.txt", ResolveChoice::Theirs),
        Outcome::Succeeded
    );
    assert_eq!(fx.read("b.txt"), "topic b.txt\n");

    // Edited by hand but not staged: still Unresolved, as in git.
    fx.write("c.txt", "merged by hand\n");
    app.refresh();
    assert_eq!(app.unresolved(), ["c.txt"]);
    assert_eq!(
        app.continue_(),
        refused(ConflictRefusal::Unresolved { remaining: 1 })
    );
    assert_eq!(
        app.resolve("c.txt", ResolveChoice::Manual),
        Outcome::Succeeded
    );
    assert!(app.conflicts().all_resolved());
    assert_eq!(app.resolved(), ["a.txt", "b.txt", "c.txt"]);

    // The editor in the isolated config is `false`: this only works
    // because the runner sets GIT_EDITOR=true.
    assert_eq!(app.continue_(), Outcome::Succeeded);
    assert_eq!(*app.phase(), Phase::Clean);
    assert!(!fx.git_dir().join("MERGE_HEAD").exists());
    assert_eq!(fx.git(&["log", "-1", "--format=%p"]).split(' ').count(), 2);
    assert_eq!(fx.git(&["show", "HEAD:a.txt"]), "main a.txt");
    assert_eq!(fx.git(&["show", "HEAD:b.txt"]), "topic b.txt");
    assert_eq!(fx.git(&["show", "HEAD:c.txt"]), "merged by hand");
    assert_eq!(fx.git(&["status", "--porcelain"]), "");
    // Law 9: nobody asked for `side`.
    assert_eq!(fx.rev("side"), side_before);

    // The merge commit leaves a marker an undo stops at.
    assert_eq!(
        app.request(Event::UndoRequested),
        Outcome::Refused(Refusal::NotUndoable(Irreversible::Continued))
    );
}

#[test]
fn aborting_a_merge_restores_head_branch_and_index_exactly() {
    let fx = Fx::new();
    fx.write("a.txt", "base\n");
    fx.write("b.txt", "base\n");
    fx.git(&["add", "."]);
    fx.git(&["commit", "-q", "-m", "base"]);
    fx.git(&["branch", "side"]);
    fx.git(&["switch", "-q", "-c", "topic"]);
    fx.commit("a.txt", "topic\n", "topic a");
    fx.git(&["switch", "-q", "main"]);
    fx.commit("a.txt", "main\n", "main a");
    let before = fx.picture();
    fx.git_stops(&["merge", "--no-edit", "topic"]);

    let mut app = fx.app();
    assert_eq!(app.unresolved(), ["a.txt"]);
    // Half way through: one file already resolved and staged.
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Theirs),
        Outcome::Succeeded
    );
    assert_ne!(fx.picture(), before);

    assert_eq!(app.request(Event::AbortRequested), Outcome::Succeeded);
    assert_eq!(*app.phase(), Phase::Clean);
    assert_eq!(fx.picture(), before);
    assert!(!fx.git_dir().join("MERGE_HEAD").exists());
}

#[test]
fn abort_is_allowed_with_every_file_still_open() {
    let fx = Fx::new();
    fx.conflicting_merge(&["a.txt", "b.txt"]);
    let mut app = fx.app();
    assert_eq!(app.unresolved().len(), 2);
    assert_eq!(app.request(Event::AbortRequested), Outcome::Succeeded);
    assert_eq!(*app.phase(), Phase::Clean);
    assert_eq!(fx.read("a.txt"), "main a.txt\n");
    assert_eq!(fx.git(&["status", "--porcelain"]), "");
}

#[test]
fn skip_is_not_available_in_a_merge() {
    let fx = Fx::new();
    fx.conflicting_merge(&["a.txt"]);
    let mut app = fx.app();
    let before = fx.picture();
    assert_eq!(
        app.request(Event::SkipRequested),
        refused(ConflictRefusal::SkipNotAvailable)
    );
    assert_eq!(fx.picture(), before);
}

#[test]
fn a_file_that_is_not_conflicted_is_not_resolved() {
    let fx = Fx::new();
    fx.conflicting_merge(&["a.txt"]);
    let mut app = fx.app();
    let before = fx.picture();
    assert_eq!(
        app.resolve("nope.txt", ResolveChoice::Manual),
        refused(ConflictRefusal::NotConflicted)
    );
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Ours),
        Outcome::Succeeded
    );
    // Once staged there are no stages left to take a side from.
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Theirs),
        refused(ConflictRefusal::AlreadyResolved)
    );
    assert_eq!(fx.read("a.txt"), "main a.txt\n");
    assert_ne!(fx.picture(), before);
}

#[test]
fn a_lock_held_by_another_process_holds_the_abort_back() {
    let fx = Fx::new();
    fx.conflicting_merge(&["a.txt"]);
    let mut app = fx.app();
    // Law 12: someone else is running git.
    fs::write(fx.git_dir().join("index.lock"), "").unwrap();
    app.refresh();
    let before = fx.picture();
    assert_eq!(app.request(Event::AbortRequested), Outcome::Nothing);
    assert_eq!(app.state.ops.queued.len(), 1);
    assert_eq!(fx.picture(), before);
    assert!(fx.git_dir().join("MERGE_HEAD").exists());
    // Released: the read that follows lets the abort run.
    fs::remove_file(fx.git_dir().join("index.lock")).unwrap();
    app.refresh();
    assert!(app.state.ops.queued.is_empty());
    assert_eq!(*app.phase(), Phase::Clean);
}

#[test]
fn paths_with_glob_characters_are_resolved_literally() {
    let fx = Fx::new();
    // `x[ab].txt` is a valid name everywhere, and also a pattern that
    // matches `xa.txt`.
    fx.conflicting_merge(&["x[ab].txt", "xa.txt", "sp ace.txt"]);
    let mut app = fx.app();
    assert_eq!(app.unresolved().len(), 3);
    assert_eq!(
        app.resolve("x[ab].txt", ResolveChoice::Theirs),
        Outcome::Succeeded
    );
    // The pattern did not take `xa.txt` with it.
    assert_eq!(app.unresolved(), ["sp ace.txt", "xa.txt"]);
    assert_eq!(fx.read("x[ab].txt"), "topic x[ab].txt\n");
    assert_eq!(fx.read("xa.txt").lines().next(), Some("<<<<<<< HEAD"));
    assert_eq!(
        app.resolve("sp ace.txt", ResolveChoice::Ours),
        Outcome::Succeeded
    );
    assert_eq!(app.unresolved(), ["xa.txt"]);
}

// --- delete/modify, binary, content ----------------------------------------

/// `main` and `topic` diverge on `f.txt`: one deletes it, the other changes
/// it. `main` merges `topic`.
fn delete_modify(fx: &Fx, main_deletes: bool) {
    fx.write("f.txt", "base\n");
    fx.write("keep.txt", "keep\n");
    fx.git(&["add", "."]);
    fx.git(&["commit", "-q", "-m", "base"]);
    fx.git(&["switch", "-q", "-c", "topic"]);
    if main_deletes {
        fx.commit("f.txt", "topic edit\n", "topic edits");
    } else {
        fx.git(&["rm", "-q", "f.txt"]);
        fx.git(&["commit", "-q", "-m", "topic deletes"]);
    }
    fx.git(&["switch", "-q", "main"]);
    if main_deletes {
        fx.git(&["rm", "-q", "f.txt"]);
        fx.git(&["commit", "-q", "-m", "main deletes"]);
    } else {
        fx.commit("f.txt", "main edit\n", "main edits");
    }
    fx.git_stops(&["merge", "--no-edit", "topic"]);
}

#[test]
fn delete_modify_conflicts_resolve_either_way_and_continue() {
    // (main deletes, choice, expected file after: None = gone)
    let cases = [
        (true, ResolveChoice::Ours, None),
        (true, ResolveChoice::Theirs, Some("topic edit")),
        (false, ResolveChoice::Ours, Some("main edit")),
        (false, ResolveChoice::Theirs, None),
    ];
    for (main_deletes, choice, expected) in cases {
        let fx = Fx::new();
        delete_modify(&fx, main_deletes);
        let mut app = fx.app();
        let kind = if main_deletes {
            ConflictKind::DeletedByUs
        } else {
            ConflictKind::DeletedByThem
        };
        assert_eq!(
            app.conflicts()
                .get(&RepoPath("f.txt".into()))
                .unwrap()
                .state,
            FileState::Unresolved(kind)
        );
        assert_eq!(
            app.resolve("f.txt", choice),
            Outcome::Succeeded,
            "{kind:?} {choice:?}"
        );
        assert!(app.conflicts().all_resolved());
        assert_eq!(app.continue_(), Outcome::Succeeded, "{kind:?} {choice:?}");
        assert_eq!(*app.phase(), Phase::Clean);
        match expected {
            None => assert!(!fx.dir.join("f.txt").exists(), "{kind:?} {choice:?}"),
            Some(text) => assert_eq!(fx.read("f.txt"), format!("{text}\n")),
        }
        assert_eq!(fx.git(&["status", "--porcelain"]), "");
    }
}

#[test]
fn deleting_by_hand_resolves_a_delete_modify_conflict() {
    let fx = Fx::new();
    delete_modify(&fx, false);
    let mut app = fx.app();
    fs::remove_file(fx.dir.join("f.txt")).unwrap();
    assert_eq!(
        app.resolve("f.txt", ResolveChoice::Manual),
        Outcome::Succeeded
    );
    assert_eq!(app.continue_(), Outcome::Succeeded);
    assert!(!fx.dir.join("f.txt").exists());
}

#[test]
fn binary_conflicts_are_flagged_and_resolve_by_side() {
    let fx = Fx::new();
    fx.write_bytes("img.bin", &[0, 1, 2, 3]);
    fx.git(&["add", "."]);
    fx.git(&["commit", "-q", "-m", "base"]);
    fx.git(&["switch", "-q", "-c", "topic"]);
    fx.write_bytes("img.bin", &[0, 1, 4, 4, 4]);
    fx.git(&["commit", "-q", "-am", "topic"]);
    fx.git(&["switch", "-q", "main"]);
    fx.write_bytes("img.bin", &[0, 1, 3, 3]);
    fx.git(&["commit", "-q", "-am", "main"]);
    fx.git_stops(&["merge", "--no-edit", "topic"]);

    let content = read_conflict_content(&fx.paths(), &RepoPath("img.bin".into())).unwrap();
    assert!(content.is_binary());
    assert_eq!(content.base, StageContent::Binary { size: 4 });
    assert_eq!(content.ours, StageContent::Binary { size: 4 });
    assert_eq!(content.theirs, StageContent::Binary { size: 5 });

    let mut app = fx.app();
    assert_eq!(
        app.resolve("img.bin", ResolveChoice::Theirs),
        Outcome::Succeeded
    );
    assert_eq!(fs::read(fx.dir.join("img.bin")).unwrap(), [0, 1, 4, 4, 4]);
    assert_eq!(app.continue_(), Outcome::Succeeded);
}

#[test]
fn conflict_content_has_the_three_stages_byte_for_byte() {
    let fx = Fx::new();
    // CRLF on every side, and a file added on both sides (no base).
    fx.write_bytes("crlf.txt", b"one\r\ntwo\r\nthree\r\n");
    fx.git(&["add", "."]);
    fx.git(&["commit", "-q", "-m", "base"]);
    fx.git(&["switch", "-q", "-c", "topic"]);
    fx.write_bytes("crlf.txt", b"one\r\nTOPIC\r\nthree\r\n");
    fx.write_bytes("both.txt", b"topic added\r\n");
    fx.git(&["add", "."]);
    fx.git(&["commit", "-q", "-m", "topic"]);
    fx.git(&["switch", "-q", "main"]);
    fx.write_bytes("crlf.txt", b"one\r\nMAIN\r\nthree\r\n");
    fx.write_bytes("both.txt", b"main added\r\n");
    fx.git(&["add", "."]);
    fx.git(&["commit", "-q", "-m", "main"]);
    fx.git_stops(&["merge", "--no-edit", "topic"]);

    let text = |s: &str| StageContent::Text(s.into());
    let crlf = read_conflict_content(&fx.paths(), &RepoPath("crlf.txt".into())).unwrap();
    assert_eq!(crlf.base, text("one\r\ntwo\r\nthree\r\n"));
    assert_eq!(crlf.ours, text("one\r\nMAIN\r\nthree\r\n"));
    assert_eq!(crlf.theirs, text("one\r\nTOPIC\r\nthree\r\n"));
    assert!(!crlf.is_binary());

    let both = read_conflict_content(&fx.paths(), &RepoPath("both.txt".into())).unwrap();
    assert_eq!(both.base, StageContent::Absent);
    assert_eq!(both.ours, text("main added\r\n"));
    assert_eq!(both.theirs, text("topic added\r\n"));

    // A path that is not conflicted has no stages to read.
    assert!(read_conflict_content(&fx.paths(), &RepoPath("nope.txt".into())).is_err());
}

#[test]
fn delete_modify_content_has_an_absent_side() {
    let fx = Fx::new();
    delete_modify(&fx, true);
    let content = read_conflict_content(&fx.paths(), &RepoPath("f.txt".into())).unwrap();
    assert_eq!(content.ours, StageContent::Absent);
    assert_eq!(content.base, StageContent::Text("base\n".into()));
    assert_eq!(content.theirs, StageContent::Text("topic edit\n".into()));
}

// --- rebase ----------------------------------------------------------------

#[test]
fn rebase_advances_step_by_step_with_continue_and_skip() {
    let fx = Fx::new();
    fx.conflicting_rebase();
    let side_before = fx.rev("side");
    let main_before = fx.rev("main");
    let mut app = fx.app();

    assert_eq!(
        *app.phase().operation().as_ref().unwrap(),
        Operation::Rebase { step: 1, total: 3 }
    );
    assert_eq!(app.unresolved(), ["a.txt"]);
    assert_eq!(
        app.continue_(),
        refused(ConflictRefusal::Unresolved { remaining: 1 })
    );

    // Step 1: resolve by hand and continue; step 2 stops on the same file.
    fx.write("a.txt", "r1\n");
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Manual),
        Outcome::Succeeded
    );
    assert_eq!(app.resolved(), ["a.txt"]);
    let outcome = app.continue_();
    assert!(
        matches!(outcome, Outcome::Failed(ErrorClass::Expected, _)),
        "{outcome:?}"
    );
    assert_eq!(
        *app.phase().operation().as_ref().unwrap(),
        Operation::Rebase { step: 2, total: 3 }
    );
    // The new conflict is a new list: step 1's file is not carried over as
    // Resolved while it is Unresolved again.
    assert_eq!(app.unresolved(), ["a.txt"]);
    assert!(app.resolved().is_empty());

    // Step 2: skip the commit. Step 3 stops.
    // (Skipping lands on step 3's conflict, which is an expected failure.)
    let outcome = app.request(Event::SkipRequested);
    assert!(
        matches!(outcome, Outcome::Failed(ErrorClass::Expected, _)),
        "{outcome:?}"
    );
    assert_eq!(
        *app.phase().operation().as_ref().unwrap(),
        Operation::Rebase { step: 3, total: 3 }
    );
    assert_eq!(app.unresolved(), ["a.txt"]);

    // Step 3: take the replayed commit's side, then finish.
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Theirs),
        Outcome::Succeeded
    );
    assert_eq!(app.continue_(), Outcome::Succeeded);
    assert_eq!(*app.phase(), Phase::Clean);
    assert_eq!(fx.git(&["symbolic-ref", "--short", "HEAD"]), "topic");
    assert_eq!(fx.git(&["log", "--format=%s", "main..topic"]), "t3\nt1");
    assert_eq!(fx.git(&["show", "topic~1:a.txt"]), "r1");
    assert_eq!(fx.git(&["show", "topic:a.txt"]), "t3");
    // Law 9: only `topic` moved.
    assert_eq!(fx.rev("side"), side_before);
    assert_eq!(fx.rev("main"), main_before);
}

#[test]
fn aborting_a_rebase_restores_the_branch_head_and_index_exactly() {
    let fx = Fx::new();
    // The picture before the rebase: `topic` checked out, clean.
    fx.commit("a.txt", "0\n", "base");
    fx.git(&["branch", "side"]);
    fx.git(&["switch", "-q", "-c", "topic"]);
    for n in 1..=3 {
        fx.commit("a.txt", &format!("t{n}\n"), &format!("t{n}"));
    }
    fx.git(&["switch", "-q", "main"]);
    fx.commit("a.txt", "main\n", "main");
    fx.git(&["switch", "-q", "topic"]);
    let before = fx.picture();
    fx.git_stops(&["rebase", "main"]);

    let mut app = fx.app();
    // Go one step in, so the abort has real work to undo.
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Ours),
        Outcome::Succeeded
    );
    let _ = app.continue_();
    assert_eq!(
        *app.phase().operation().as_ref().unwrap(),
        Operation::Rebase { step: 2, total: 3 }
    );

    // Abort is allowed whatever the files say.
    assert_eq!(app.request(Event::AbortRequested), Outcome::Succeeded);
    assert_eq!(*app.phase(), Phase::Clean);
    assert_eq!(fx.picture(), before);
    assert!(!fx.git_dir().join("rebase-merge").exists());
}

#[test]
fn a_skip_meant_for_one_step_is_not_applied_to_the_next() {
    let fx = Fx::new();
    fx.conflicting_rebase();
    let mut app = fx.app();
    // Two skips in a row, the second queued while the first is waiting for
    // the lock: the second was asked at step 1 and must not skip step 2.
    fs::write(fx.git_dir().join("index.lock"), "").unwrap();
    app.refresh();
    app.deliver(Event::SkipRequested);
    app.deliver(Event::SkipRequested);
    assert_eq!(app.state.ops.queued.len(), 2);
    fs::remove_file(fx.git_dir().join("index.lock")).unwrap();
    app.refresh();
    // The first skip ran (step 2 now) and the second was turned down.
    assert_eq!(
        *app.phase().operation().as_ref().unwrap(),
        Operation::Rebase { step: 2, total: 3 }
    );
    assert_eq!(
        app.state.notice,
        Some(Notice::Refused {
            action: Action::Skip,
            reason: Refusal::Conflict(ConflictRefusal::PhaseChanged)
        })
    );
}

#[test]
fn an_empty_resolution_drops_the_commit_and_moves_on() {
    let fx = Fx::new();
    fx.conflicting_rebase();
    let mut app = fx.app();
    // Taking `main`'s side makes step 1 change nothing: git drops the
    // commit when continuing, and stops at the next conflict.
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Ours),
        Outcome::Succeeded
    );
    assert!(app.conflicts().all_resolved());
    let outcome = app.continue_();
    assert!(
        matches!(outcome, Outcome::Failed(ErrorClass::Expected, _)),
        "{outcome:?}"
    );
    assert_eq!(
        *app.phase().operation().as_ref().unwrap(),
        Operation::Rebase { step: 2, total: 3 }
    );
    assert_eq!(fx.git(&["rev-list", "--count", "main..HEAD"]), "0");
}

// --- cherry-pick and revert -------------------------------------------------

#[test]
fn cherry_pick_conflicts_resolve_continue_and_abort() {
    let fx = Fx::new();
    fx.commit("a.txt", "base\n", "base");
    fx.git(&["branch", "side"]);
    fx.git(&["switch", "-q", "-c", "topic"]);
    fx.commit("a.txt", "topic\n", "topic change");
    fx.git(&["switch", "-q", "main"]);
    fx.commit("a.txt", "main\n", "main change");
    let before = fx.picture();
    fx.git_stops(&["cherry-pick", "topic"]);

    let mut app = fx.app();
    assert!(matches!(app.phase(), Phase::CherryPicking { .. }));
    assert_eq!(app.unresolved(), ["a.txt"]);
    assert_eq!(
        app.request(Event::SkipRequested),
        refused(ConflictRefusal::SkipNotAvailable)
    );

    // Abort puts everything back...
    assert_eq!(app.request(Event::AbortRequested), Outcome::Succeeded);
    assert_eq!(fx.picture(), before);
    assert_eq!(*app.phase(), Phase::Clean);

    // ...and the same pick, resolved, continues to a new commit that keeps
    // the picked commit's message.
    fx.git_stops(&["cherry-pick", "topic"]);
    app.refresh();
    fx.write("a.txt", "both\n");
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Manual),
        Outcome::Succeeded
    );
    assert_eq!(app.continue_(), Outcome::Succeeded);
    assert_eq!(*app.phase(), Phase::Clean);
    assert_eq!(fx.git(&["log", "-1", "--format=%s"]), "topic change");
    assert_eq!(fx.git(&["show", "HEAD:a.txt"]), "both");
    assert_eq!(fx.git(&["rev-list", "--count", "HEAD"]), "3");
}

#[test]
fn revert_conflicts_resolve_continue_and_abort() {
    let fx = Fx::new();
    fx.commit("a.txt", "1\n", "one");
    fx.commit("a.txt", "2\n", "two");
    fx.commit("a.txt", "3\n", "three");
    fx.git(&["branch", "side"]);
    let before = fx.picture();
    fx.git_stops(&["revert", "--no-edit", "HEAD~1"]);

    let mut app = fx.app();
    assert!(matches!(app.phase(), Phase::Reverting { .. }));
    assert_eq!(app.unresolved(), ["a.txt"]);
    assert_eq!(app.request(Event::AbortRequested), Outcome::Succeeded);
    assert_eq!(fx.picture(), before);

    fx.git_stops(&["revert", "--no-edit", "HEAD~1"]);
    app.refresh();
    fx.write("a.txt", "reverted by hand\n");
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Manual),
        Outcome::Succeeded
    );
    assert_eq!(app.continue_(), Outcome::Succeeded);
    assert_eq!(*app.phase(), Phase::Clean);
    assert_eq!(fx.git(&["log", "-1", "--format=%s"]), "Revert \"two\"");
    assert_eq!(fx.git(&["show", "HEAD:a.txt"]), "reverted by hand");
}

// --- P2: cherry-pick and revert started by the app ----------------------------

fn oid(fx: &Fx, name: &str) -> Oid {
    Oid::from_hex(&fx.rev(name)).unwrap()
}

/// `main` and `topic` both change a.txt from "base"; `topic` also adds b.txt.
fn diverged(fx: &Fx) {
    fx.commit("a.txt", "base\n", "base");
    fx.git(&["branch", "side"]);
    fx.git(&["switch", "-q", "-c", "topic"]);
    fx.commit("a.txt", "topic\n", "topic change");
    fx.git(&["switch", "-q", "main"]);
    fx.commit("a.txt", "main\n", "main change");
}

/// Refs, `HEAD` and index entries (what law 8 restores).
fn refs_and_index(fx: &Fx) -> (String, String, String) {
    let p = fx.picture();
    (p.refs, p.head, p.index)
}

#[test]
fn a_clean_cherry_pick_commits_and_undo_restores_refs_and_index() {
    let fx = Fx::new();
    fx.commit("a.txt", "base\n", "base");
    fx.git(&["branch", "side"]);
    fx.git(&["switch", "-q", "-c", "topic"]);
    fx.commit("b.txt", "new\n", "add b");
    fx.git(&["switch", "-q", "main"]);
    let before = refs_and_index(&fx);
    let mut app = fx.app();
    assert_eq!(
        app.request(Event::CherryPickRequested(oid(&fx, "topic"))),
        Outcome::Succeeded
    );
    assert_eq!(*app.phase(), Phase::Clean);
    assert_eq!(fx.git(&["log", "-1", "--format=%s"]), "add b");
    assert_eq!(fx.git(&["rev-list", "--count", "HEAD"]), "2");
    // Law 9: no other branch moved.
    assert_eq!(fx.rev("side"), fx.rev("HEAD~1"));
    assert_eq!(app.request(Event::UndoRequested), Outcome::Succeeded);
    assert_eq!(refs_and_index(&fx), before);
}

#[test]
fn a_clean_revert_commits_and_undo_restores_refs_and_index() {
    let fx = Fx::new();
    fx.commit("a.txt", "1\n", "one");
    fx.commit("b.txt", "x\n", "two");
    let before = refs_and_index(&fx);
    let mut app = fx.app();
    assert_eq!(
        app.request(Event::RevertRequested(oid(&fx, "HEAD"))),
        Outcome::Succeeded
    );
    assert_eq!(fx.git(&["log", "-1", "--format=%s"]), "Revert \"two\"");
    assert!(!fx.dir.join("b.txt").exists());
    assert_eq!(app.request(Event::UndoRequested), Outcome::Succeeded);
    assert_eq!(refs_and_index(&fx), before);
}

#[test]
fn a_conflicting_cherry_pick_pauses_aborts_whole_and_resolves_to_a_commit() {
    let fx = Fx::new();
    diverged(&fx);
    let before = fx.picture();
    let mut app = fx.app();
    let topic = oid(&fx, "topic");
    let outcome = app.request(Event::CherryPickRequested(topic.clone()));
    assert!(
        matches!(outcome, Outcome::Failed(ErrorClass::Expected, _)),
        "{outcome:?}"
    );
    assert!(matches!(app.phase(), Phase::CherryPicking { .. }));
    assert_eq!(app.unresolved(), ["a.txt"]);
    // The failed start left no undo point, and nothing new starts meanwhile.
    assert!(app.state.undo.entries.is_empty());
    assert_eq!(
        app.request(Event::CherryPickRequested(topic.clone())),
        Outcome::Refused(Refusal::RepoBusy)
    );
    assert_eq!(
        app.request(Event::RevertRequested(topic.clone())),
        Outcome::Refused(Refusal::RepoBusy)
    );
    // Continue is blocked until the file is resolved.
    assert_eq!(
        app.continue_(),
        refused(ConflictRefusal::Unresolved { remaining: 1 })
    );
    // Abort restores everything exactly.
    assert_eq!(app.request(Event::AbortRequested), Outcome::Succeeded);
    assert_eq!(fx.picture(), before);
    assert_eq!(*app.phase(), Phase::Clean);

    // Again, resolved by hand and continued.
    let _ = app.request(Event::CherryPickRequested(topic));
    fx.write("a.txt", "both\n");
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Manual),
        Outcome::Succeeded
    );
    assert_eq!(app.continue_(), Outcome::Succeeded);
    assert_eq!(*app.phase(), Phase::Clean);
    assert_eq!(fx.git(&["log", "-1", "--format=%s"]), "topic change");
    assert_eq!(fx.git(&["show", "HEAD:a.txt"]), "both");
    // Undo stops at the continued conflict (abort was the way back).
    assert_eq!(
        app.request(Event::UndoRequested),
        Outcome::Refused(Refusal::NotUndoable(Irreversible::Continued))
    );
}

#[test]
fn a_conflicting_revert_pauses_aborts_whole_and_resolves_to_a_commit() {
    let fx = Fx::new();
    fx.commit("a.txt", "1\n", "one");
    fx.commit("a.txt", "2\n", "two");
    fx.commit("a.txt", "3\n", "three");
    fx.git(&["branch", "side"]);
    let before = fx.picture();
    let mut app = fx.app();
    let two = oid(&fx, "HEAD~1");
    let outcome = app.request(Event::RevertRequested(two.clone()));
    assert!(
        matches!(outcome, Outcome::Failed(ErrorClass::Expected, _)),
        "{outcome:?}"
    );
    assert!(matches!(app.phase(), Phase::Reverting { .. }));
    assert!(app.state.undo.entries.is_empty());
    assert_eq!(app.request(Event::AbortRequested), Outcome::Succeeded);
    assert_eq!(fx.picture(), before);

    let _ = app.request(Event::RevertRequested(two));
    fx.write("a.txt", "reverted by hand\n");
    assert_eq!(
        app.resolve("a.txt", ResolveChoice::Manual),
        Outcome::Succeeded
    );
    assert_eq!(app.continue_(), Outcome::Succeeded);
    assert_eq!(*app.phase(), Phase::Clean);
    assert_eq!(fx.git(&["log", "-1", "--format=%s"]), "Revert \"two\"");
    assert_eq!(fx.git(&["show", "HEAD:a.txt"]), "reverted by hand");
}

#[test]
fn a_pick_git_turns_down_changes_nothing() {
    let fx = Fx::new();
    diverged(&fx);
    // A merge commit needs `-m`: git refuses before touching anything.
    fx.git(&[
        "merge", "--no-ff", "-q", "-X", "ours", "-m", "merge", "topic",
    ]);
    let merge = oid(&fx, "HEAD");
    fx.git(&["reset", "-q", "--hard", "HEAD~1"]);
    let before = fx.picture();
    let mut app = fx.app();
    // The merge commit is still in the object database.
    let outcome = app.request(Event::CherryPickRequested(merge));
    assert!(
        matches!(outcome, Outcome::Failed(ErrorClass::Expected, _)),
        "{outcome:?}"
    );
    assert_eq!(*app.phase(), Phase::Clean);
    assert_eq!(fx.picture(), before);
    // Changes to tracked files (staged or not) are refused by the core, so
    // an abort can never rewrite an edit; untracked files do not matter.
    for stage in [true, false] {
        fx.write("a.txt", "edited\n");
        if stage {
            fx.git(&["add", "a.txt"]);
        }
        let before = fx.picture();
        app.refresh();
        for event in [
            Event::CherryPickRequested(oid(&fx, "topic")),
            Event::RevertRequested(oid(&fx, "HEAD")),
        ] {
            assert_eq!(
                app.request(event),
                Outcome::Refused(Refusal::UncommittedChanges)
            );
        }
        assert_eq!(fx.picture(), before);
        fx.git(&["checkout", "-q", "HEAD", "--", "a.txt"]);
    }
}

#[test]
fn a_pick_is_refused_when_the_index_moved_since_the_read() {
    let fx = Fx::new();
    fx.commit("a.txt", "base\n", "base");
    fx.git(&["switch", "-q", "-c", "topic"]);
    fx.commit("b.txt", "new\n", "add b");
    fx.git(&["switch", "-q", "main"]);
    let mut app = fx.app();
    // Someone stages a file behind the app's back; no watcher event arrives.
    fx.write("c.txt", "c\n");
    fx.git(&["add", "c.txt"]);
    let (state, commands) = step(
        app.state.clone(),
        Event::CherryPickRequested(oid(&fx, "topic")),
    );
    app.state = state;
    let outcome = commands
        .into_iter()
        .find_map(|command| match command {
            Command::RunGit { op, args, expect } => {
                let (sender, events) = mpsc::channel();
                app.runner.run(op, args, expect, sender);
                loop {
                    if let Event::OpFinished(_, outcome) = events
                        .recv_timeout(Duration::from_secs(60))
                        .expect("finished")
                    {
                        break Some(outcome);
                    }
                }
            }
            _ => None,
        })
        .expect("a run");
    assert!(
        matches!(&outcome, OpOutcome::Failed(f) if f.class == ErrorClass::PreconditionChanged),
        "{outcome:?}"
    );
    assert_eq!(fx.git(&["rev-list", "--count", "HEAD"]), "1");
}

// --- a conflict made by the app's own pull ------------------------------------

#[test]
fn a_pull_that_conflicts_opens_the_flow_and_pauses_the_network() {
    let fx = Fx::new();
    let origin = fx.root.join("origin.git");
    fx.git_in(
        &fx.root,
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    fx.commit("a.txt", "base\n", "base");
    fx.git(&["remote", "add", "origin", origin.to_str().unwrap()]);
    fx.git(&["push", "-q", "-u", "origin", "main"]);
    // A teammate changes the line on the remote...
    let bob = fx.root.join("bob");
    fx.git_in(&fx.root, &["clone", "-q", origin.to_str().unwrap(), "bob"]);
    fs::write(bob.join("a.txt"), "bob\n").unwrap();
    fx.git_in(&bob, &["commit", "-q", "-am", "bob"]);
    fx.git_in(&bob, &["push", "-q", "origin", "main"]);
    // ...and so do we.
    fx.commit("a.txt", "alice\n", "alice");
    let before = fx.picture();

    let mut app = fx.app();
    let origin_main = RemoteBranch {
        remote: "origin".into(),
        branch: BranchName("main".into()),
    };
    let outcome = app.request(Event::PullRequested {
        from: origin_main.clone(),
        mode: PullMode::Merge,
    });
    assert!(
        matches!(outcome, Outcome::Failed(ErrorClass::Expected, _)),
        "{outcome:?}"
    );
    // The flow opens from the snapshot, not from the notice.
    assert!(matches!(app.phase(), Phase::Merging { .. }));
    assert_eq!(app.unresolved(), ["a.txt"]);

    // Fetch, pull and push are paused while it lasts.
    for event in [
        Event::FetchRequested {
            remote: "origin".into(),
        },
        Event::PullRequested {
            from: origin_main.clone(),
            mode: PullMode::Merge,
        },
        Event::PushRequested {
            to: origin_main,
            force_with_lease: false,
            set_upstream: false,
        },
    ] {
        assert_eq!(app.request(event), Outcome::Refused(Refusal::RepoBusy));
    }

    // Abort restores what the pull started from, remote-tracking refs
    // included (the fetch part of the pull is not undone, and is not a
    // branch).
    assert_eq!(app.request(Event::AbortRequested), Outcome::Succeeded);
    let after = fx.picture();
    assert_eq!(after.head, before.head);
    assert_eq!(after.index, before.index);
    assert_eq!(after.status, before.status);
    assert_eq!(after.files, before.files);
    assert_eq!(fx.rev("main"), fx.git(&["rev-parse", "refs/heads/main"]));
    assert_eq!(fx.git(&["log", "-1", "--format=%s", "main"]), "alice");
}

/// Taking a side is two git runs; when the first fails the second must not
/// run, or the worktree file would be staged as it is.
#[test]
fn a_failed_checkout_does_not_stage_the_file() {
    let fx = Fx::new();
    fx.commit("a.txt", "base\n", "base");
    // Untracked: git has no side of it to check out, but could stage it.
    fx.write("new.txt", "new\n");
    let before = fx.picture();
    let expect = shell::precondition::current(&fx.paths()).unwrap();
    let runner = GitRunner::new(Git::detect("git").unwrap(), fx.paths()).with_env(fx.env.clone());
    let (sender, events) = mpsc::channel();
    runner.run(
        invoke_core::OpId(1),
        invoke_core::GitArgs::ResolveTakeSide {
            path: RepoPath("new.txt".into()),
            side: invoke_core::Side::Ours,
        },
        expect,
        sender,
    );
    let outcome = loop {
        if let Event::OpFinished(_, outcome) = events.recv_timeout(Duration::from_secs(60)).unwrap()
        {
            break outcome;
        }
    };
    assert!(matches!(outcome, OpOutcome::Failed(_)), "{outcome:?}");
    assert_eq!(fx.picture(), before);
    assert_eq!(fx.git(&["diff", "--cached", "--name-only"]), "");
}

#[test]
fn conflicts_helper_sanity() {
    // The fixture's `Conflicts` type is the core's: sorted and unique.
    let list = Conflicts::from_parts(
        [
            (RepoPath("b".into()), ConflictKind::BothAdded),
            (RepoPath("a".into()), ConflictKind::BothModified),
        ],
        [RepoPath("a".into())],
    );
    assert_eq!(list.files().len(), 2);
    assert_eq!(list.unresolved_count(), 2);
}
