//! Backlog task 11, core half, end to end: the real core drives the real
//! shell on generated temp repos, patches come from the pure `diff` crate,
//! and real git is the oracle. System and global git config are shut out.
//!
//! Owns the real-git sides of law 4 (staging never changes the worktree
//! file), law 5 (staging every hunk makes the index equal the worktree) and
//! law 6 (stage then unstage restores the index), and checks the status
//! read. Task 24 will fold these into the model-based harness.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use diff::{DiffOptions, Direction, Mode, PatchSpec, Pick, build_patch, diff_text};
use invoke_core::{
    Action, ChangeSet, Command as Cmd, ErrorClass, Event, FileChange, IndexId, Notice, OpOutcome,
    Patch, Refusal, RepoId, RepoPath, RepoState, Selection, StageRequest, open, step,
};
use proptest::prelude::*;
use shell::precondition::RepoPaths;
use shell::undo::Snapshotted;
use shell::{Git, InputsError, Source, StagingRunner, UndoRunner, load_inputs, read::read_event};

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
            "invoke-git-staging-{}-{}-{nanos}",
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

    fn output(&self, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = self.output(args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write(&self, path: &str, bytes: &str) {
        let full = self.paths.workdir.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, bytes).unwrap();
    }

    fn worktree_bytes(&self, path: &str) -> Vec<u8> {
        fs::read(self.paths.workdir.join(path)).unwrap()
    }

    /// The blob the index holds for `path`, byte for byte.
    fn index_bytes(&self, path: &str) -> Option<Vec<u8>> {
        let output = self.output(&["cat-file", "blob", &format!(":{path}")]);
        output.status.success().then_some(output.stdout)
    }

    /// The index entries (mode, blob, stage, path), the exact thing law 6
    /// restores.
    fn index_entries(&self) -> String {
        self.git(&["ls-files", "-s"])
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// What the user picked in the UI.
#[derive(Clone, Debug)]
enum Choice {
    File,
    Hunk(usize),
    Lines { hunk: usize, lines: Vec<usize> },
}

/// The real core driving the real shell, one command at a time.
struct Driver<'a> {
    repo: &'a TempRepo,
    state: RepoState,
    staging: StagingRunner,
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
            staging: StagingRunner::new(git(), repo.paths.clone()).with_env(repo.env.clone()),
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
            Cmd::ApplyPatchToIndex {
                op,
                patch,
                expect_index,
            } => self.deliver(self.staging.apply(op, &patch, &expect_index)),
            Cmd::ReplayUndo(replay) => self.deliver(self.undo.replay(&replay)),
            Cmd::RunGit { op, args, expect } => {
                let runner =
                    shell::GitRunner::new(Git::detect("git").unwrap(), self.repo.paths.clone())
                        .with_env(self.repo.env.clone());
                let (send, receive) = std::sync::mpsc::channel();
                runner.run(op, args, expect, send);
                while let Ok(event) = receive.recv() {
                    let finished = matches!(event, Event::OpFinished(..));
                    self.deliver(event);
                    if finished {
                        break;
                    }
                }
            }
            Cmd::PruneUndo(ops) => self.undo.prune(&ops),
            other => panic!("unexpected command {other:?}"),
        }
    }

    /// What the watcher sends after the repo changed behind our back.
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

    /// Builds the patch from the files as they are now, the way the UI
    /// will, and sends it. `None` when the selection changes nothing.
    fn request(&mut self, path: &str, direction: Direction, choice: &Choice) -> Option<()> {
        let (source, based_on, patch) = self.build(path, direction, choice)?;
        let _ = source;
        let selection = match choice {
            Choice::File => Selection::File(RepoPath(path.into())),
            Choice::Hunk(hunk) => Selection::Hunk {
                path: RepoPath(path.into()),
                hunk: *hunk,
            },
            Choice::Lines { hunk, lines } => Selection::Lines {
                path: RepoPath(path.into()),
                hunk: *hunk,
                lines: lines.clone(),
            },
        };
        let request = StageRequest {
            selection,
            patch: Patch(patch),
            based_on,
        };
        self.deliver(match direction {
            Direction::Stage => Event::StageRequested(request),
            Direction::Unstage => Event::UnstageRequested(request),
            Direction::Discard => unreachable!("discard is tested in discard.rs"),
        });
        Some(())
    }

    fn build(
        &self,
        path: &str,
        direction: Direction,
        choice: &Choice,
    ) -> Option<(Source, IndexId, String)> {
        build_for(&self.repo.paths, path, direction, choice)
    }

    /// Stages or unstages and requires the write to have succeeded.
    fn apply(&mut self, path: &str, direction: Direction, choice: &Choice) -> bool {
        if self.request(path, direction, choice).is_none() {
            return false;
        }
        assert_eq!(
            self.last_outcome(),
            Some(OpOutcome::Succeeded),
            "{direction:?} {choice:?} on {path}: {:?}",
            self.state.notice
        );
        true
    }
}

fn source_of(direction: Direction) -> Source {
    match direction {
        Direction::Stage | Direction::Discard => Source::IndexToWorktree,
        Direction::Unstage => Source::HeadToIndex,
    }
}

/// Reads the sides, diffs them and builds the patch, like the UI will.
fn build_for(
    paths: &RepoPaths,
    path: &str,
    direction: Direction,
    choice: &Choice,
) -> Option<(Source, IndexId, String)> {
    let source = source_of(direction);
    let inputs = load_inputs(paths, &RepoPath(path.into()), source).unwrap();
    let diff = diff_text(
        inputs.old.clone().unwrap_or_default(),
        inputs.new.clone().unwrap_or_default(),
        &DiffOptions::default(),
    );
    let spec = PatchSpec {
        path,
        direction,
        old_exists: inputs.old.is_some(),
        new_exists: inputs.new.is_some(),
        mode: if inputs.executable {
            Mode::Executable
        } else {
            Mode::Regular
        },
    };
    let pick = match choice {
        Choice::File => Pick::File,
        Choice::Hunk(hunk) => Pick::Hunk(*hunk),
        Choice::Lines { hunk, lines } => Pick::Lines { hunk: *hunk, lines },
    };
    if diff.hunks.is_empty() && !matches!(choice, Choice::File) {
        return None; // nothing left to pick from
    }
    let patch = build_patch(&diff, &spec, &pick).unwrap()?;
    Some((source, inputs.based_on, patch))
}

/// Law 4 and 5 for one file: stage the first hunk until nothing is left.
/// The worktree file never changes; at the end the index holds exactly the
/// worktree bytes (or no entry, when the worktree file is gone).
fn stage_every_hunk_one_by_one(repo: &TempRepo, path: &str) {
    let worktree_before = fs::read(repo.paths.workdir.join(path)).ok();
    let mut driver = Driver::open(repo);
    for _ in 0..64 {
        if !driver.apply(path, Direction::Stage, &Choice::Hunk(0)) {
            break;
        }
        assert_eq!(
            fs::read(repo.paths.workdir.join(path)).ok(),
            worktree_before,
            "law 4: staging changed the worktree file"
        );
    }
    assert_eq!(
        repo.index_bytes(path),
        worktree_before,
        "law 5: index differs from the worktree after staging every hunk"
    );
}

#[test]
fn crlf_file_staged_hunk_by_hunk_matches_the_worktree() {
    let repo = TempRepo::new();
    let base: String = (0..30).map(|i| format!("line {i}\r\n")).collect();
    repo.write("w.txt", &base);
    repo.commit_all("base");
    let edited = base
        .replace("line 2\r\n", "LINE 2\r\n")
        .replace("line 20\r\n", "line 20\r\nextra\r\n")
        .replace("line 28\r\n", "");
    repo.write("w.txt", &edited);
    stage_every_hunk_one_by_one(&repo, "w.txt");
    assert_eq!(repo.git(&["diff", "--stat"]), "");
}

#[test]
fn no_final_newline_survives_staging_in_both_directions() {
    let repo = TempRepo::new();
    repo.write("n.txt", "a\nb\nc");
    repo.commit_all("base");
    repo.write("n.txt", "a\nb\nc\nd");
    stage_every_hunk_one_by_one(&repo, "n.txt");
    assert_eq!(repo.index_bytes("n.txt").unwrap(), b"a\nb\nc\nd");

    // Back to a file that ends with a newline.
    repo.write("n.txt", "a\nb\nc\nd\n");
    stage_every_hunk_one_by_one(&repo, "n.txt");
    assert_eq!(repo.index_bytes("n.txt").unwrap(), b"a\nb\nc\nd\n");
}

#[test]
fn unicode_and_spaces_in_paths_stage_and_unstage() {
    let repo = TempRepo::new();
    let path = "dir/ünï côdé 日本.txt";
    repo.write(path, "alpha\nbeta\n");
    repo.commit_all("base");
    repo.write(path, "alpha\nBETA\ngamma\n");
    let mut driver = Driver::open(&repo);
    assert!(driver.apply(path, Direction::Stage, &Choice::File));
    assert_eq!(repo.index_bytes(path).unwrap(), b"alpha\nBETA\ngamma\n");
    driver.outside_change();
    assert!(driver.apply(path, Direction::Unstage, &Choice::File));
    assert_eq!(repo.index_bytes(path).unwrap(), b"alpha\nbeta\n");
    assert_eq!(repo.worktree_bytes(path), b"alpha\nBETA\ngamma\n");
}

#[test]
fn new_deleted_and_empty_files_stage_by_patch() {
    let repo = TempRepo::new();
    repo.write("gone.txt", "bye\n");
    repo.write("keep.txt", "k\n");
    repo.commit_all("base");
    fs::remove_file(repo.paths.workdir.join("gone.txt")).unwrap();
    repo.write("fresh dir/new.txt", "x\r\ny");
    repo.write("empty.txt", "");
    let mut driver = Driver::open(&repo);
    for path in ["gone.txt", "fresh dir/new.txt", "empty.txt"] {
        driver.outside_change();
        assert!(
            driver.apply(path, Direction::Stage, &Choice::File),
            "{path}"
        );
    }
    assert_eq!(repo.index_bytes("gone.txt"), None);
    assert_eq!(repo.index_bytes("fresh dir/new.txt").unwrap(), b"x\r\ny");
    assert_eq!(repo.index_bytes("empty.txt").unwrap(), b"");
    assert_eq!(repo.git(&["diff", "--stat"]), "");

    // And the same three back out of the index.
    for path in ["gone.txt", "fresh dir/new.txt", "empty.txt"] {
        driver.outside_change();
        assert!(
            driver.apply(path, Direction::Unstage, &Choice::File),
            "{path}"
        );
    }
    assert_eq!(repo.git(&["diff", "--cached", "--stat"]), "");
    assert_eq!(repo.index_bytes("gone.txt").unwrap(), b"bye\n");
}

#[cfg(unix)]
#[test]
fn a_new_executable_file_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let repo = TempRepo::new();
    repo.write("seed", "s\n");
    repo.commit_all("base");
    repo.write("run.sh", "#!/bin/sh\n");
    let full = repo.paths.workdir.join("run.sh");
    fs::set_permissions(&full, fs::Permissions::from_mode(0o755)).unwrap();
    let mut driver = Driver::open(&repo);
    assert!(driver.apply("run.sh", Direction::Stage, &Choice::File));
    assert!(
        repo.index_entries().contains("100755"),
        "{}",
        repo.index_entries()
    );
}

/// Stage lines, then the index must differ from the worktree exactly by
/// what was not selected: stage the rest and it must match.
#[test]
fn line_staging_leaves_the_rest_for_a_second_stage() {
    let repo = TempRepo::new();
    repo.write("f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n");
    repo.commit_all("base");
    repo.write("f.txt", "1\nTWO\n3\n4\n5\n6\n7\n8\nNINE\n9b\n");
    let mut driver = Driver::open(&repo);
    // The first hunk's lines: pick the added "TWO" only.
    let inputs = load_inputs(
        &repo.paths,
        &RepoPath("f.txt".into()),
        Source::IndexToWorktree,
    )
    .unwrap();
    let diff = diff_text(
        inputs.old.unwrap(),
        inputs.new.unwrap(),
        &DiffOptions::default(),
    );
    let two = diff.hunks[0]
        .lines
        .iter()
        .position(|l| diff.line_text(l) == "TWO")
        .unwrap();
    assert!(driver.apply(
        "f.txt",
        Direction::Stage,
        &Choice::Lines {
            hunk: 0,
            lines: vec![two]
        }
    ));
    assert_eq!(
        repo.index_bytes("f.txt").unwrap(),
        b"1\n2\nTWO\n3\n4\n5\n6\n7\n8\n9\n"
    );
    assert_eq!(
        repo.worktree_bytes("f.txt"),
        b"1\nTWO\n3\n4\n5\n6\n7\n8\nNINE\n9b\n"
    );
}

#[test]
fn staging_is_undone_by_undo() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\n");
    repo.commit_all("base");
    repo.write("f.txt", "a\nB\n");
    let before = repo.index_entries();
    let mut driver = Driver::open(&repo);
    assert!(driver.apply("f.txt", Direction::Stage, &Choice::File));
    assert_ne!(repo.index_entries(), before);
    assert_eq!(driver.state.undo.entries.len(), 1);
    assert_eq!(driver.state.undo.entries[0].point.label, "Stage f.txt");
    driver.deliver(Event::UndoRequested);
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(repo.index_entries(), before);
    assert_eq!(repo.worktree_bytes("f.txt"), b"a\nB\n");
}

// ---- refusals -----------------------------------------------------------

#[test]
fn a_patch_built_before_the_index_moved_is_refused_and_changes_nothing() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\n");
    repo.commit_all("base");
    repo.write("f.txt", "a\nB\n");
    let mut driver = Driver::open(&repo);
    // Build now, send later: someone else stages in between.
    let (_, based_on, patch) =
        build_for(&repo.paths, "f.txt", Direction::Stage, &Choice::File).unwrap();
    repo.git(&["add", "f.txt"]);
    let after_outside = repo.index_entries();
    driver.outside_change();
    driver.deliver(Event::StageRequested(StageRequest {
        selection: Selection::File(RepoPath("f.txt".into())),
        patch: Patch(patch),
        based_on,
    }));
    assert_eq!(
        driver.state.notice,
        Some(Notice::Refused {
            action: Action::Stage,
            reason: Refusal::StaleDiff
        })
    );
    assert_eq!(repo.index_entries(), after_outside);
}

#[test]
fn the_runner_refuses_when_the_index_or_lock_changed() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\n");
    repo.commit_all("base");
    repo.write("f.txt", "a\nB\n");
    let (_, based_on, patch) =
        build_for(&repo.paths, "f.txt", Direction::Stage, &Choice::File).unwrap();
    let runner = StagingRunner::new(Git::detect("git").unwrap(), repo.paths.clone())
        .with_env(repo.env.clone());
    let before = repo.index_entries();
    let op = invoke_core::OpId(7);

    // Another process holds the lock.
    let lock = repo.paths.git_dir.join("index.lock");
    fs::write(&lock, "").unwrap();
    let event = runner.apply(op, &Patch(patch.clone()), &based_on);
    fs::remove_file(&lock).unwrap();
    let Event::OpFinished(_, OpOutcome::Failed(failure)) = event else {
        panic!("{event:?}");
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);

    // The index moved.
    repo.git(&["add", "f.txt"]);
    repo.git(&["reset", "-q"]);
    repo.write("g.txt", "g\n");
    repo.git(&["add", "g.txt"]);
    let event = runner.apply(op, &Patch(patch), &based_on);
    let Event::OpFinished(_, OpOutcome::Failed(failure)) = event else {
        panic!("{event:?}");
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    assert_ne!(repo.index_entries(), before);
    assert_eq!(repo.index_bytes("f.txt").unwrap(), b"a\nb\n");
}

#[test]
fn a_patch_that_does_not_fit_fails_the_check_and_leaves_the_index_alone() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\n");
    repo.commit_all("base");
    let runner = StagingRunner::new(Git::detect("git").unwrap(), repo.paths.clone())
        .with_env(repo.env.clone());
    let current = shell::precondition::current(&repo.paths).unwrap();
    let bad =
        "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n a\n-WRONG\n+c\n";
    let event = runner.apply(invoke_core::OpId(1), &Patch(bad.into()), &current.index);
    let Event::OpFinished(_, OpOutcome::Failed(failure)) = event else {
        panic!("{event:?}");
    };
    assert_eq!(failure.class, ErrorClass::Bug);
    assert!(failure.details.contains("f.txt"), "{}", failure.details);
    assert_eq!(repo.index_bytes("f.txt").unwrap(), b"a\nb\n");
}

#[test]
fn binary_and_non_utf8_files_are_reported_not_diffed() {
    let repo = TempRepo::new();
    repo.write("seed", "s\n");
    repo.commit_all("base");
    fs::write(repo.paths.workdir.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
    fs::write(repo.paths.workdir.join("latin.txt"), [0xE9u8, b'\n']).unwrap();
    for path in ["bin.dat", "latin.txt"] {
        let result = load_inputs(&repo.paths, &RepoPath(path.into()), Source::IndexToWorktree);
        assert!(
            matches!(result, Err(InputsError::NotText(_))),
            "{path}: {result:?}"
        );
    }
}

#[test]
fn whole_file_stage_unstage_and_undo_keep_worktree_and_intent_to_add() {
    use invoke_core::{FileAction, FileRequest};

    let repo = TempRepo::new();
    repo.write("base", "base\n");
    repo.commit_all("base");
    for (name, bytes) in [("binary[1]", &b"x\0\x01"[..]), ("encoded", &b"\xffabc"[..])] {
        fs::write(repo.paths.workdir.join(name), bytes).unwrap();
        repo.git(&["--literal-pathspecs", "add", "-N", "--", name]);
        let before = fs::read(repo.paths.git_dir.join("index")).unwrap();
        let mut driver = Driver::open(&repo);
        let path = RepoPath(name.into());
        for action in [FileAction::Stage, FileAction::Unstage] {
            let based_on = shell::precondition::current(&repo.paths).unwrap().index;
            driver.deliver(Event::FileRequested(FileRequest {
                path: path.clone(),
                action: action.clone(),
                based_on,
            }));
            assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
            assert_eq!(repo.worktree_bytes(name), bytes);
            assert_eq!(
                repo.index_bytes(name).as_deref(),
                if action == FileAction::Stage {
                    Some(bytes)
                } else {
                    None
                },
            );
        }
        for _ in 0..2 {
            driver.deliver(Event::UndoRequested);
            assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
        }
        assert_eq!(fs::read(repo.paths.git_dir.join("index")).unwrap(), before);
        assert_eq!(repo.worktree_bytes(name), bytes);
    }
}

#[test]
fn whole_file_unstage_mode_only_and_undo_restore_exact_index() {
    use invoke_core::{FileAction, FileRequest};

    let repo = TempRepo::new();
    repo.write("script", "script\n");
    repo.commit_all("base");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            repo.paths.workdir.join("script"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    #[cfg(not(unix))]
    repo.git(&["update-index", "--chmod=+x", "script"]);
    let mut driver = Driver::open(&repo);
    let request = |action| {
        Event::FileRequested(FileRequest {
            path: RepoPath("script".into()),
            action,
            based_on: shell::precondition::current(&repo.paths).unwrap().index,
        })
    };
    #[cfg(unix)]
    {
        driver.deliver(request(FileAction::Stage));
        assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    }
    assert!(repo.index_entries().starts_with("100755 "));
    let before = fs::read(repo.paths.git_dir.join("index")).unwrap();
    driver.deliver(request(FileAction::Unstage));
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    assert!(repo.index_entries().starts_with("100644 "));
    driver.deliver(Event::UndoRequested);
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(fs::read(repo.paths.git_dir.join("index")).unwrap(), before);
    assert_eq!(repo.worktree_bytes("script"), b"script\n");
}

#[test]
fn whole_file_undo_keeps_staged_blobs_and_split_index_reachable() {
    use invoke_core::{FileAction, FileRequest};

    let repo = TempRepo::new();
    repo.write("file", "base\n");
    repo.commit_all("base");
    fs::write(repo.paths.workdir.join("file"), b"staged\0").unwrap();
    repo.git(&["add", "--", "file"]);
    repo.git(&["update-index", "--split-index"]);
    let shared = fs::read_dir(&repo.paths.git_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("sharedindex.")
        })
        .unwrap();
    let before = fs::read(repo.paths.git_dir.join("index")).unwrap();
    repo.git(&["config", "core.splitIndex", "false"]);
    let mut driver = Driver::open(&repo);
    driver.deliver(Event::FileRequested(FileRequest {
        path: RepoPath("file".into()),
        action: FileAction::Unstage,
        based_on: shell::precondition::current(&repo.paths).unwrap().index,
    }));
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    if shared.exists() {
        fs::remove_file(&shared).unwrap();
    }
    repo.git(&["prune", "--expire=now"]);
    driver.deliver(Event::UndoRequested);
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(fs::read(repo.paths.git_dir.join("index")).unwrap(), before);
    assert_eq!(repo.index_bytes("file").as_deref(), Some(&b"staged\0"[..]));
    assert!(shared.is_file());
}

// ---- the status read ----------------------------------------------------

#[test]
fn status_lists_staged_unstaged_and_untracked_sorted_and_respects_ignores() {
    let repo = TempRepo::new();
    repo.write(".gitignore", "*.log\nbuild/\n");
    repo.write("a.txt", "a\n");
    repo.write("src/keep.rs", "k\n");
    repo.write("src/gone.rs", "g\n");
    repo.write("src/both.rs", "1\n");
    repo.write("ünï/ç.txt", "u\n");
    repo.commit_all("base");

    repo.write("a.txt", "a2\n"); // unstaged modification
    fs::remove_file(repo.paths.workdir.join("src/gone.rs")).unwrap(); // unstaged delete
    repo.write("src/both.rs", "2\n");
    repo.git(&["add", "src/both.rs"]); // staged ...
    repo.write("src/both.rs", "3\n"); // ... and modified again
    repo.write("staged_new.txt", "n\n");
    repo.git(&["add", "staged_new.txt"]);
    repo.git(&["rm", "-q", "--cached", "ünï/ç.txt"]); // staged delete, file remains
    repo.write("untracked/deep/file.txt", "t\n");
    repo.write("debug.log", "ignored\n");
    repo.write("build/out.o", "ignored\n");

    let disk = shell::read_state(&repo.paths).unwrap();
    let rows: Vec<(String, Option<FileChange>, Option<FileChange>)> = disk
        .status
        .entries
        .iter()
        .map(|e| (e.path.0.clone(), e.staged.clone(), e.unstaged.clone()))
        .collect();
    use FileChange::*;
    assert_eq!(
        rows,
        vec![
            ("a.txt".into(), None, Some(Modified)),
            ("src/both.rs".into(), Some(Modified), Some(Modified)),
            ("src/gone.rs".into(), None, Some(Deleted)),
            ("staged_new.txt".into(), Some(Added), None),
            ("untracked/deep/file.txt".into(), None, Some(Untracked)),
            // The file stays on disk but left the index: staged delete + untracked.
            ("ünï/ç.txt".into(), Some(Deleted), Some(Untracked)),
        ]
    );
}

#[test]
fn reading_status_neither_takes_the_lock_nor_changes_the_index() {
    let repo = TempRepo::new();
    repo.write("a.txt", "a\n");
    repo.commit_all("base");
    repo.write("a.txt", "changed\n");
    let index = repo.paths.git_dir.join("index");
    let before = fs::read(&index).unwrap();
    // Even with someone else holding the lock the read goes through.
    let lock = repo.paths.git_dir.join("index.lock");
    fs::write(&lock, "").unwrap();
    let disk = shell::read_state(&repo.paths).unwrap();
    fs::remove_file(&lock).unwrap();
    assert_eq!(disk.status.entries.len(), 1);
    assert_eq!(fs::read(&index).unwrap(), before);
}

#[test]
fn an_unborn_repo_lists_everything_as_untracked_or_added() {
    let repo = TempRepo::new();
    repo.write("a.txt", "a\n");
    repo.write("b.txt", "b\n");
    repo.git(&["add", "a.txt"]);
    let disk = shell::read_state(&repo.paths).unwrap();
    let rows: Vec<_> = disk
        .status
        .entries
        .iter()
        .map(|e| (e.path.0.as_str(), e.staged.clone(), e.unstaged.clone()))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("a.txt", Some(FileChange::Added), None),
            ("b.txt", None, Some(FileChange::Untracked)),
        ]
    );
}

/// Picks the lines of a diff by text and kind, for a hand-made selection.
fn lines_where(
    repo: &TempRepo,
    path: &str,
    source: Source,
    wanted: &[(diff::LineKind, &str)],
) -> Vec<usize> {
    let inputs = load_inputs(&repo.paths, &RepoPath(path.into()), source).unwrap();
    let diff = diff_text(
        inputs.old.unwrap_or_default(),
        inputs.new.unwrap_or_default(),
        &DiffOptions::default(),
    );
    diff.hunks[0]
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| wanted.contains(&(l.kind, diff.line_text(l))))
        .map(|(i, _)| i)
        .collect()
}

/// Unstaging some lines leaves the other staged changes in the index.
#[test]
fn unstaging_a_line_selection_keeps_the_other_changes_staged() {
    use diff::LineKind::{Added, Removed};
    let repo = TempRepo::new();
    repo.write("f.txt", "1\n2\n3\n4\n5\n");
    repo.commit_all("base");
    repo.write("f.txt", "1\nTWO\n3\nFOUR\n5\n");
    let mut driver = Driver::open(&repo);
    assert!(driver.apply("f.txt", Direction::Stage, &Choice::File));
    let lines = lines_where(
        &repo,
        "f.txt",
        Source::HeadToIndex,
        &[(Removed, "2"), (Added, "TWO")],
    );
    assert_eq!(lines.len(), 2);
    assert!(driver.apply(
        "f.txt",
        Direction::Unstage,
        &Choice::Lines { hunk: 0, lines }
    ));
    assert_eq!(repo.index_bytes("f.txt").unwrap(), b"1\n2\n3\nFOUR\n5\n");
    assert_eq!(repo.worktree_bytes("f.txt"), b"1\nTWO\n3\nFOUR\n5\n");
}

/// A line after one without a final newline: the newline goes along.
#[test]
fn staging_after_an_unterminated_last_line_stages_its_newline_too() {
    use diff::LineKind::Added;
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb");
    repo.commit_all("base");
    repo.write("f.txt", "a\nb\nc\n");
    let mut driver = Driver::open(&repo);
    let lines = lines_where(&repo, "f.txt", Source::IndexToWorktree, &[(Added, "c")]);
    assert!(driver.apply("f.txt", Direction::Stage, &Choice::Lines { hunk: 0, lines }));
    assert_eq!(repo.index_bytes("f.txt").unwrap(), b"a\nb\nc\n");
    assert_eq!(repo.worktree_bytes("f.txt"), b"a\nb\nc\n");
}

/// The mirror: restoring HEAD's unterminated line takes the lines after it.
#[test]
fn unstaging_an_unterminated_last_line_unstages_the_lines_after_it() {
    use diff::LineKind::Removed;
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb");
    repo.commit_all("base");
    repo.write("f.txt", "a\nb\nc\n");
    let mut driver = Driver::open(&repo);
    assert!(driver.apply("f.txt", Direction::Stage, &Choice::File));
    let lines = lines_where(&repo, "f.txt", Source::HeadToIndex, &[(Removed, "b")]);
    assert!(driver.apply(
        "f.txt",
        Direction::Unstage,
        &Choice::Lines { hunk: 0, lines }
    ));
    assert_eq!(repo.index_bytes("f.txt").unwrap(), b"a\nb");
}

// ---- generated files: laws 4, 5, 6 with random texts and selections -----

const POOL: [&str; 9] = ["", "a", "b", "}", "-x", "+z", "é日本", "same", "same"];

#[derive(Debug, Clone)]
struct Text {
    lines: Vec<(usize, bool)>, // pool index, CRLF
    final_newline: bool,
}

impl Text {
    fn render(&self) -> String {
        let mut out = String::new();
        for (i, (pool, crlf)) in self.lines.iter().enumerate() {
            out.push_str(POOL[*pool]);
            if i + 1 != self.lines.len() || self.final_newline {
                out.push_str(if *crlf { "\r\n" } else { "\n" });
            }
        }
        out
    }
}

fn text() -> impl Strategy<Value = Text> {
    (
        prop::collection::vec((0..POOL.len(), prop::bool::weighted(0.3)), 0..14),
        prop::bool::weighted(0.6),
    )
        .prop_map(|(lines, final_newline)| Text {
            lines,
            final_newline,
        })
}

fn pair() -> impl Strategy<Value = (String, String)> {
    (
        text(),
        prop::collection::vec((any::<prop::sample::Index>(), 0..3u8, 0..POOL.len()), 0..6),
        prop::bool::weighted(0.3),
    )
        .prop_map(|(a, edits, flip_final)| {
            let mut b = a.clone();
            for (at, kind, pool) in edits {
                let len = b.lines.len();
                match kind {
                    0 => b
                        .lines
                        .insert(if len == 0 { 0 } else { at.index(len + 1) }, (pool, false)),
                    1 if len > 0 => {
                        b.lines.remove(at.index(len));
                    }
                    _ if len > 0 => b.lines[at.index(len)] = (pool, false),
                    _ => {}
                }
            }
            if flip_final {
                b.final_newline = !b.final_newline;
            }
            (a.render(), b.render())
        })
}

const PATHS: [&str; 3] = ["f.txt", "dir/é f.txt", "a/b/c.rs"];

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, failure_persistence: None, ..ProptestConfig::default() })]

    /// Laws 4 and 5: whatever the file looks like, staging every hunk one
    /// by one leaves the worktree alone and makes the index equal it.
    #[test]
    fn law_4_and_5_every_hunk_staged((old, new) in pair(), path in 0..PATHS.len()) {
        let repo = TempRepo::new();
        repo.write(PATHS[path], &old);
        repo.commit_all("base");
        repo.write(PATHS[path], &new);
        stage_every_hunk_one_by_one(&repo, PATHS[path]);
    }

    /// Law 4 and 6: stage any selection of lines of any hunk, then unstage
    /// the file; the index entries are exactly what they were, and the
    /// worktree was never touched.
    #[test]
    fn law_6_stage_then_unstage_restores_the_index(
        (old, new) in pair(),
        path in 0..PATHS.len(),
        hunk in any::<prop::sample::Index>(),
        bits in prop::collection::vec(any::<bool>(), 32),
    ) {
        let path = PATHS[path];
        let repo = TempRepo::new();
        repo.write(path, &old);
        repo.commit_all("base");
        repo.write(path, &new);
        let entries_before = repo.index_entries();
        let worktree_before = repo.worktree_bytes(path);
        let mut driver = Driver::open(&repo);

        let inputs = load_inputs(&repo.paths, &RepoPath(path.into()), Source::IndexToWorktree).unwrap();
        let diff = diff_text(inputs.old.unwrap(), inputs.new.unwrap(), &DiffOptions::default());
        if diff.hunks.is_empty() {
            return Ok(());
        }
        let hunk = hunk.index(diff.hunks.len());
        let lines: Vec<usize> = (0..diff.hunks[hunk].lines.len()).filter(|&i| bits[i % bits.len()]).collect();

        if driver.apply(path, Direction::Stage, &Choice::Lines { hunk, lines }) {
            prop_assert_eq!(repo.worktree_bytes(path), worktree_before.clone(), "law 4");
            driver.outside_change();
            // `false`: the staged change was a no-op, nothing to unstage.
            driver.apply(path, Direction::Unstage, &Choice::File);
            prop_assert_eq!(repo.index_entries(), entries_before, "law 6");
        }
        prop_assert_eq!(repo.worktree_bytes(path), worktree_before, "law 4");
    }
}
