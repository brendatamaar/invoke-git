//! Backlog task 13, core half, end to end: the real core drives the real
//! shell on generated temp repos, discard patches come from the pure `diff`
//! crate, and real git is the oracle. System and global git config are shut
//! out.
//!
//! Owns the real-git side of law 7 (discarding a selection changes only the
//! selected lines) and the discard part of law 8 (undo restores the exact
//! bytes). The pure side of law 7 is in `crates/diff/tests/patch.rs`.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use diff::{
    DiffOptions, Direction, LineKind, Mode, PatchSpec, Pick, TextDiff, build_patch, diff_text,
};
use invoke_core::{
    Action, ChangeSet, Command as Cmd, DiscardRequest, ErrorClass, Event, Failure, IndexId, Notice,
    OpId, OpOutcome, Patch, Refusal, RepoId, RepoPath, RepoState, Selection, open, step,
};
use proptest::prelude::*;
use shell::precondition::RepoPaths;
use shell::undo::Snapshotted;
use shell::{DiscardRunner, Git, InputsError, Source, UndoRunner, load_inputs, read::read_event};

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
            "invoke-git-discard-{}-{}-{nanos}",
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

    fn write(&self, path: &str, text: &str) {
        self.write_bytes(path, text.as_bytes());
    }

    fn write_bytes(&self, path: &str, bytes: &[u8]) {
        let full = self.paths.workdir.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, bytes).unwrap();
    }

    fn worktree_bytes(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.paths.workdir.join(path)).ok()
    }

    fn exists(&self, path: &str) -> bool {
        self.paths.workdir.join(path).exists()
    }

    fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }

    /// Everything a discard must not touch: HEAD, the index entries, and
    /// every ref outside `refs/invoke-git/`.
    fn untouched_state(&self) -> (String, String, String) {
        (
            self.git(&["rev-parse", "HEAD"]),
            self.git(&["ls-files", "-s"]),
            self.git(&[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
            ]),
        )
    }

    fn index_file_bytes(&self) -> Vec<u8> {
        fs::read(self.paths.git_dir.join("index")).unwrap()
    }

    /// Our own refs, the keep-alive of saved content.
    fn undo_refs(&self) -> Vec<String> {
        self.git(&["for-each-ref", "--format=%(refname)", "refs/invoke-git"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Every file under the worktree (not `.git`), with its bytes.
    fn all_files(&self) -> Vec<(String, Vec<u8>)> {
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.file_name().is_some_and(|name| name == ".git") {
                    continue;
                }
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    let name = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((name, fs::read(&path).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.paths.workdir, &self.paths.workdir, &mut out);
        out.sort();
        out
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
    discard: DiscardRunner,
    undo: UndoRunner,
    /// Operations whose undo snapshot failed: the discard is not run.
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
            discard: DiscardRunner::new(git(), repo.paths.clone()).with_env(repo.env.clone()),
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
            Cmd::ApplyPatchToWorktree {
                op,
                patch,
                path,
                expect_index,
                expect_file,
            } => {
                if self.skip.remove(&op) {
                    return;
                }
                let done =
                    self.discard
                        .apply(op, &patch, &path, &expect_index, expect_file.as_ref());
                self.deliver(done);
            }
            Cmd::ReplayUndo(replay) => self.deliver(self.undo.replay(&replay)),
            Cmd::RunGit { op, args, expect } => {
                if self.skip.remove(&op) {
                    return;
                }
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

    /// Reads the file as the UI would, builds the request and sends it.
    /// `None` when the selection changes nothing.
    fn request(&mut self, path: &str, choice: &Choice) -> Option<()> {
        self.deliver(Event::NoticeDismissed);
        let request = build_request(&self.repo.paths, path, choice)?;
        self.deliver(Event::DiscardRequested(request));
        Some(())
    }

    /// Discards and requires the write to have succeeded.
    fn discard(&mut self, path: &str, choice: &Choice) -> bool {
        if self.request(path, choice).is_none() {
            return false;
        }
        assert_eq!(
            self.last_outcome(),
            Some(OpOutcome::Succeeded),
            "discard {choice:?} on {path}: {:?}",
            self.state.notice
        );
        true
    }

    /// Asks for an undo and requires it to have succeeded.
    fn undo(&mut self) {
        self.deliver(Event::NoticeDismissed);
        self.outside_change();
        self.deliver(Event::UndoRequested);
        assert_eq!(
            self.last_outcome(),
            Some(OpOutcome::Succeeded),
            "undo: {:?}",
            self.state.notice
        );
    }
}

fn whole_file_request(repo: &TempRepo, name: &str) -> Event {
    use invoke_core::{FileAction, FileRequest};

    let path = RepoPath(name.into());
    let inputs = shell::discard::file_inputs(&repo.paths, &path).unwrap();
    Event::FileRequested(FileRequest {
        path,
        action: FileAction::Discard { file: inputs.file },
        based_on: inputs.based_on,
    })
}

#[test]
fn whole_file_discard_and_undo_preserve_exact_bytes_and_index() {
    let repo = TempRepo::new();
    repo.git(&["config", "core.autocrlf", "true"]);
    let cases = [
        ("binary", Some(&b"old\0\x01"[..]), &b"changed\0\x02"[..]),
        ("non-utf8", Some(&b"old\xff"[..]), &b"changed\xff"[..]),
        ("crlf", Some(&b"old\r\n"[..]), &b"changed\r\nmixed\n"[..]),
        ("untracked[1]", None, &b"untracked\0\xff"[..]),
        ("intent", None, &b"intent\0"[..]),
    ];
    for (name, old, _) in cases {
        if let Some(old) = old {
            repo.write_bytes(name, old);
        }
    }
    repo.commit_all("base");
    for (name, old, edited) in cases {
        repo.write_bytes(name, edited);
        if name == "intent" {
            repo.git(&["add", "-N", "--", name]);
        }
        let index = repo.index_file_bytes();
        let mut driver = Driver::open(&repo);
        driver.deliver(whole_file_request(&repo, name));
        assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded), "{name}");
        assert_eq!(repo.worktree_bytes(name).as_deref(), old, "{name}");
        assert_eq!(repo.index_file_bytes(), index);
        driver.undo();
        assert_eq!(repo.worktree_bytes(name).as_deref(), Some(edited));
        assert_eq!(repo.index_file_bytes(), index);
    }
}

#[test]
fn whole_file_discard_refuses_a_changed_file_or_held_index_lock() {
    let repo = TempRepo::new();
    repo.write_bytes("target", b"base\0");
    repo.commit_all("base");
    repo.write_bytes("target", b"first\0");
    let mut driver = Driver::open(&repo);
    let request = whole_file_request(&repo, "target");
    repo.write_bytes("target", b"later\0");
    driver.deliver(request);
    assert!(matches!(
        driver.last_outcome(),
        Some(OpOutcome::Failed(Failure {
            class: ErrorClass::PreconditionChanged,
            ..
        }))
    ));
    assert_eq!(
        repo.worktree_bytes("target").as_deref(),
        Some(&b"later\0"[..])
    );
    assert!(driver.state.undo.entries.is_empty());

    let request = whole_file_request(&repo, "target");
    let path = repo.paths.git_dir.join("index.lock");
    fs::write(&path, b"external lock").unwrap();
    driver.deliver(request);
    assert!(matches!(
        driver.last_outcome(),
        Some(OpOutcome::Failed(Failure {
            class: ErrorClass::PreconditionChanged,
            ..
        }))
    ));
    assert_eq!(fs::read(path).unwrap(), b"external lock");
    assert_eq!(
        repo.worktree_bytes("target").as_deref(),
        Some(&b"later\0"[..])
    );
}

#[test]
fn whole_file_actions_keep_symlink_targets_and_undo_restores_the_link() {
    use invoke_core::{FileAction, FileRequest};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    #[cfg(windows)]
    use std::os::windows::fs::symlink_file as symlink;

    let repo = TempRepo::new();
    let root = &repo.paths.workdir;
    let link = root.join("link");
    repo.git(&["config", "core.symlinks", "true"]);
    repo.write("first", "untouched first");
    repo.write("second", "untouched second");
    let created = symlink("first", &link);
    #[cfg(windows)]
    if created
        .as_ref()
        .is_err_and(|error| error.raw_os_error() == Some(1314))
    {
        eprintln!("Native symlinks require Windows developer mode or a symlink privilege");
        return;
    }
    created.unwrap();
    repo.commit_all("base");
    fs::remove_file(&link).unwrap();
    symlink("second", &link).unwrap();
    let mut driver = Driver::open(&repo);
    for (action, target) in [
        (FileAction::Stage, "second"),
        (FileAction::Unstage, "first"),
    ] {
        driver.deliver(Event::FileRequested(FileRequest {
            path: RepoPath("link".into()),
            action,
            based_on: shell::precondition::current(&repo.paths).unwrap().index,
        }));
        assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
        assert_eq!(repo.git(&["show", ":link"]), target);
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("second"));
    }
    driver.deliver(whole_file_request(&repo, "link"));
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("first"));
    driver.undo();
    assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("second"));
    assert_eq!(
        repo.worktree_bytes("first").as_deref(),
        Some(&b"untouched first"[..])
    );
    assert_eq!(
        repo.worktree_bytes("second").as_deref(),
        Some(&b"untouched second"[..])
    );
}

#[test]
#[cfg(unix)]
fn whole_file_discard_and_undo_restore_mode() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TempRepo::new();
    let script = repo.paths.workdir.join("script");
    repo.write("script", "script\n");
    repo.commit_all("base");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let mut driver = Driver::open(&repo);
    driver.deliver(whole_file_request(&repo, "script"));
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(script.metadata().unwrap().permissions().mode() & 0o111, 0);
    driver.undo();
    assert_eq!(
        script.metadata().unwrap().permissions().mode() & 0o111,
        0o111
    );
}

#[test]
#[cfg(windows)]
fn whole_file_discard_and_undo_keep_git_symlink_emulation() {
    // Git for Windows uses a plain file containing the link target when
    // symlink creation is disabled. This fixture needs no extra privilege.
    let repo = TempRepo::new();
    repo.git(&["config", "core.symlinks", "false"]);
    repo.write("target", "first");
    repo.commit_all("base");
    let blob = repo.git(&["rev-parse", "HEAD:target"]);
    repo.git(&[
        "update-index",
        "--cacheinfo",
        "120000",
        blob.trim(),
        "target",
    ]);
    repo.git(&["commit", "-qm", "record symlink"]);
    repo.write("target", "second");
    let mut driver = Driver::open(&repo);
    driver.deliver(whole_file_request(&repo, "target"));
    assert_eq!(driver.last_outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(
        repo.worktree_bytes("target").as_deref(),
        Some(&b"first"[..])
    );
    driver.undo();
    assert_eq!(
        repo.worktree_bytes("target").as_deref(),
        Some(&b"second"[..])
    );
}

/// Reads the sides, diffs them and builds the request, like the UI will.
fn build_request(paths: &RepoPaths, path: &str, choice: &Choice) -> Option<DiscardRequest> {
    let inputs = load_inputs(paths, &RepoPath(path.into()), Source::IndexToWorktree).unwrap();
    let diff = diff_text(
        inputs.old.clone().unwrap_or_default(),
        inputs.new.clone().unwrap_or_default(),
        &DiffOptions::default(),
    );
    let spec = PatchSpec {
        path,
        direction: Direction::Discard,
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
        return None;
    }
    let patch = build_patch(&diff, &spec, &pick).unwrap()?;
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
    Some(DiscardRequest {
        selection,
        patch: Patch(patch),
        based_on: inputs.based_on,
        expect_file: inputs.new_blob,
    })
}

// ---- law 7: only the selected lines change ------------------------------

const POOL: [&str; 10] = [
    "", "a", "b", "}", "-x", "--- y", "+z", "é日本", "same", "same",
];

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
        prop::collection::vec((0..POOL.len(), prop::bool::weighted(0.3)), 1..14),
        prop::bool::weighted(0.6),
    )
        .prop_map(|(lines, final_newline)| Text {
            lines,
            final_newline,
        })
}

/// An index text and an edited worktree text, so hunks are small and many.
fn pair() -> impl Strategy<Value = (String, String)> {
    (
        text(),
        prop::collection::vec((any::<prop::sample::Index>(), 0..3u8, 0..POOL.len()), 1..6),
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
                    1 if len > 1 => {
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

const NAMES: [&str; 4] = [
    "f.txt",
    "dir/g.txt",
    "dir with space/ünï côdé 日本.txt",
    "deep/er/h.rs",
];

/// What the worktree must hold after discarding the picked lines, found by
/// walking the diff by the table in the spec (an index-to-worktree diff;
/// picked `+` lines go, picked `-` lines come back, everything else stays).
fn expected_worktree(diff: &TextDiff, picked: &dyn Fn(usize, usize) -> bool) -> Vec<String> {
    let file: Vec<&str> = diff.new.split_inclusive('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0;
    for (h, hunk) in diff.hunks.iter().enumerate() {
        let before = if hunk.new_len == 0 {
            hunk.new_start
        } else {
            hunk.new_start - 1
        } as usize;
        out.extend(file[cursor..before].iter().map(|line| line.to_string()));
        cursor = before;
        for (i, line) in hunk.lines.iter().enumerate() {
            let full = format!("{}{}", diff.line_text(line), line.eol.as_str());
            let on = picked(h, i);
            match line.kind {
                LineKind::Context => {
                    out.push(full);
                    cursor += 1;
                }
                LineKind::Added => {
                    cursor += 1;
                    if !on {
                        out.push(full);
                    }
                }
                LineKind::Removed => {
                    if on {
                        out.push(full);
                    }
                }
            }
        }
    }
    out.extend(file[cursor..].iter().map(|line| line.to_string()));
    out
}

/// A line without a newline may only end the file; otherwise no text can
/// hold the result, and the builder widened the selection.
fn well_formed(lines: &[String]) -> bool {
    lines
        .iter()
        .take(lines.len().saturating_sub(1))
        .all(|line| line.ends_with('\n'))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Law 7: discarding a file, a hunk or a line selection changes the
    /// worktree file exactly as the table says and nothing else (not the
    /// other files, the index or any ref); law 8: undo restores the exact
    /// bytes. CRLF, missing final newline and unicode paths are in the mix.
    #[test]
    fn law_7_discard_changes_only_selected_lines(
        (index_text, worktree_text) in pair(),
        name in 0..NAMES.len(),
        kind in 0..3u8,
        which in any::<prop::sample::Index>(),
        mask in any::<u32>(),
    ) {
        let path = NAMES[name];
        let repo = TempRepo::new();
        repo.write(path, &index_text);
        repo.write("bystander.txt", "keep\r\nme");
        repo.commit_all("base");
        repo.write(path, &worktree_text);
        // A second, unrelated, uncommitted change that must survive.
        repo.write("other.txt", "untracked\n");
        let diff = diff_text(index_text.as_str(), worktree_text.as_str(), &DiffOptions::default());
        prop_assume!(!diff.hunks.is_empty());
        let h = which.index(diff.hunks.len());
        let changed: Vec<usize> = diff.hunks[h]
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.kind != LineKind::Context)
            .map(|(i, _)| i)
            .collect();
        let chosen: Vec<usize> = changed
            .iter()
            .enumerate()
            .filter(|(n, _)| mask >> (n % 32) & 1 == 1)
            .map(|(_, i)| *i)
            .collect();
        let (choice, picked): (Choice, Box<dyn Fn(usize, usize) -> bool>) = match kind {
            0 => (Choice::File, Box::new(|_, _| true)),
            1 => (Choice::Hunk(h), Box::new(move |hh, _| hh == h)),
            _ => {
                let set = chosen.clone();
                (
                    Choice::Lines { hunk: h, lines: chosen.clone() },
                    Box::new(move |hh, i| hh == h && set.contains(&i)),
                )
            }
        };
        let picked_line = |hh: usize, i: usize| {
            diff.hunks[hh].lines[i].kind != LineKind::Context && picked(hh, i)
        };
        let expected = expected_worktree(&diff, &picked_line);

        let before = repo.untouched_state();
        let files_before = repo.all_files();
        let index_file = repo.index_file_bytes();
        let mut driver = Driver::open(&repo);
        if driver.request(path, &choice).is_none() {
            // Nothing selected: nothing may change.
            prop_assert_eq!(repo.all_files(), files_before);
            return Ok(());
        }
        prop_assert_eq!(
            driver.last_outcome(),
            Some(OpOutcome::Succeeded),
            "{:?}", driver.state.notice
        );
        let after = repo.worktree_bytes(path).unwrap_or_default();
        if well_formed(&expected) {
            let want = expected.concat().into_bytes();
            prop_assert_eq!(after, want, "law 7: wrong lines changed");
        }
        // Law 7: no other file changed, and git's own data did not move.
        let mut files_after = repo.all_files();
        files_after.retain(|(name, _)| name != path);
        let mut others_before = files_before.clone();
        others_before.retain(|(name, _)| name != path);
        prop_assert_eq!(files_after, others_before, "law 7: another file changed");
        prop_assert_eq!(repo.untouched_state(), before.clone(), "law 7: head, index or refs moved");
        prop_assert_eq!(repo.index_file_bytes(), index_file.clone(), "law 7: the index file moved");

        // Law 8 for discards: undo gives back the exact bytes.
        driver.undo();
        prop_assert_eq!(
            repo.worktree_bytes(path).unwrap(),
            worktree_text.as_bytes(),
            "law 8: undo did not restore the file"
        );
        prop_assert_eq!(repo.all_files(), files_before);
        prop_assert_eq!(repo.untouched_state(), before);
        prop_assert_eq!(repo.index_file_bytes(), index_file);
    }
}

// ---- examples -----------------------------------------------------------

#[test]
fn a_crlf_hunk_is_discarded_and_its_neighbour_stays() {
    let repo = TempRepo::new();
    let base: String = (0..30).map(|i| format!("line {i}\r\n")).collect();
    repo.write("w.txt", &base);
    repo.commit_all("base");
    let edited = base
        .replace("line 2\r\n", "LINE 2\r\n")
        .replace("line 25\r\n", "line 25\r\nextra\r\n");
    repo.write("w.txt", &edited);
    let mut driver = Driver::open(&repo);
    assert!(driver.discard("w.txt", &Choice::Hunk(1)));
    let expected = base.replace("line 2\r\n", "LINE 2\r\n");
    assert_eq!(repo.worktree_bytes("w.txt").unwrap(), expected.as_bytes());
    // The rest of the diff is still there, byte for byte.
    assert_eq!(
        repo.git(&["diff", "--stat"]),
        "w.txt | 2 +-\n 1 file changed, 1 insertion(+), 1 deletion(-)"
    );
    driver.undo();
    assert_eq!(repo.worktree_bytes("w.txt").unwrap(), edited.as_bytes());
}

#[test]
fn single_lines_of_a_crlf_file_are_discarded() {
    let repo = TempRepo::new();
    repo.write("c.txt", "a\r\nb\r\nc\r\n");
    repo.commit_all("base");
    repo.write("c.txt", "a\r\nB\r\nB2\r\nc\r\n");
    let mut driver = Driver::open(&repo);
    // Lines are [ctx a, -b, +B, +B2, ctx c]: discard only "+B2".
    let diff = diff_text(
        "a\r\nb\r\nc\r\n",
        "a\r\nB\r\nB2\r\nc\r\n",
        &DiffOptions::default(),
    );
    let at = diff.hunks[0]
        .lines
        .iter()
        .position(|line| diff.line_text(line) == "B2")
        .unwrap();
    assert!(driver.discard(
        "c.txt",
        &Choice::Lines {
            hunk: 0,
            lines: vec![at]
        }
    ));
    assert_eq!(repo.worktree_bytes("c.txt").unwrap(), b"a\r\nB\r\nc\r\n");
}

#[test]
fn a_missing_final_newline_comes_back_exactly() {
    let repo = TempRepo::new();
    repo.write("n.txt", "a\nb");
    repo.commit_all("base");
    repo.write("n.txt", "a\nb\nc\nd\n");
    let mut driver = Driver::open(&repo);
    assert!(driver.discard("n.txt", &Choice::File));
    assert_eq!(repo.worktree_bytes("n.txt").unwrap(), b"a\nb");
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
    driver.undo();
    assert_eq!(repo.worktree_bytes("n.txt").unwrap(), b"a\nb\nc\nd\n");

    // And the other way: a final newline taken away is given back.
    let repo = TempRepo::new();
    repo.write("n.txt", "a\nb\n");
    repo.commit_all("base");
    repo.write("n.txt", "a\nb\nc");
    let mut driver = Driver::open(&repo);
    assert!(driver.discard("n.txt", &Choice::File));
    assert_eq!(repo.worktree_bytes("n.txt").unwrap(), b"a\nb\n");
}

#[test]
fn unicode_and_spaces_in_paths_discard_and_undo() {
    let repo = TempRepo::new();
    let path = "dir/ünï côdé 日本.txt";
    repo.write(path, "alpha\nbeta\n");
    repo.commit_all("base");
    repo.write(path, "alpha\nBETA\ngamma\n");
    let mut driver = Driver::open(&repo);
    assert!(driver.discard(path, &Choice::File));
    assert_eq!(repo.worktree_bytes(path).unwrap(), b"alpha\nbeta\n");
    let refs = repo.undo_refs();
    assert_eq!(refs.len(), 3, "{refs:?}"); // tree, discard, discard-after
    driver.undo();
    assert_eq!(repo.worktree_bytes(path).unwrap(), b"alpha\nBETA\ngamma\n");
    assert!(repo.undo_refs().is_empty(), "{:?}", repo.undo_refs());
}

#[test]
fn an_untracked_file_is_deleted_and_undo_brings_it_back() {
    let repo = TempRepo::new();
    repo.write("seed", "s\n");
    repo.commit_all("base");
    let bytes = b"x\r\ny\0z\xff".as_slice(); // not text, not UTF-8: only discards of text go through the UI
    let _ = bytes;
    repo.write("fresh dir/new.txt", "x\r\ny");
    repo.write("empty.txt", "");
    let mut driver = Driver::open(&repo);
    for path in ["fresh dir/new.txt", "empty.txt"] {
        driver.outside_change();
        assert!(driver.discard(path, &Choice::File), "{path}");
        assert!(!repo.exists(path), "{path} is still there");
    }
    assert!(
        !repo.exists("fresh dir"),
        "the empty folder was left behind"
    );
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
    // Undo in order: the last discard first.
    driver.undo();
    assert_eq!(repo.worktree_bytes("empty.txt").unwrap(), b"");
    assert!(!repo.exists("fresh dir/new.txt"));
    driver.undo();
    assert_eq!(repo.worktree_bytes("fresh dir/new.txt").unwrap(), b"x\r\ny");
}

#[test]
fn a_deleted_file_comes_back_and_undo_removes_it_again() {
    let repo = TempRepo::new();
    repo.write("gone.txt", "bye\r\nbye");
    repo.write("keep.txt", "k\n");
    repo.commit_all("base");
    fs::remove_file(repo.paths.workdir.join("gone.txt")).unwrap();
    let mut driver = Driver::open(&repo);
    assert!(driver.discard("gone.txt", &Choice::File));
    assert_eq!(repo.worktree_bytes("gone.txt").unwrap(), b"bye\r\nbye");
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
    driver.undo();
    assert!(!repo.exists("gone.txt"));
    // (trimmed from " D gone.txt")
    assert_eq!(repo.git(&["status", "--porcelain"]), "D gone.txt");
}

#[cfg(unix)]
#[test]
fn modes_survive_discard_and_undo() {
    use std::os::unix::fs::PermissionsExt;
    // git creates executable files as 0777 minus the umask, so only the
    // executable bit is compared.
    let mode_of = |repo: &TempRepo, path: &str| {
        fs::metadata(repo.paths.workdir.join(path))
            .unwrap()
            .permissions()
            .mode()
            & 0o100
    };
    let repo = TempRepo::new();
    repo.write("run.sh", "#!/bin/sh\necho a\n");
    repo.write("gone.sh", "#!/bin/sh\n");
    repo.git(&["add", "-A"]);
    repo.git(&["update-index", "--chmod=+x", "run.sh", "gone.sh"]);
    repo.git(&["commit", "-q", "-m", "base"]);
    repo.git(&["checkout", "-q", "--", "."]);
    for path in ["run.sh", "gone.sh"] {
        fs::set_permissions(
            repo.paths.workdir.join(path),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    repo.write("run.sh", "#!/bin/sh\necho b\n");
    fs::remove_file(repo.paths.workdir.join("gone.sh")).unwrap();
    repo.write("new.sh", "#!/bin/sh\n");
    fs::set_permissions(
        repo.paths.workdir.join("new.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    let mut driver = Driver::open(&repo);
    assert!(driver.discard("run.sh", &Choice::File));
    assert_eq!(mode_of(&repo, "run.sh"), 0o100);
    driver.outside_change();
    assert!(driver.discard("gone.sh", &Choice::File));
    assert_eq!(
        mode_of(&repo, "gone.sh"),
        0o100,
        "a recreated file keeps its mode"
    );
    driver.outside_change();
    assert!(driver.discard("new.sh", &Choice::File));
    assert!(!repo.exists("new.sh"));
    driver.undo();
    assert_eq!(mode_of(&repo, "new.sh"), 0o100, "undo restores the mode");
    driver.undo();
    assert!(!repo.exists("gone.sh"));
    driver.undo();
    assert_eq!(
        repo.worktree_bytes("run.sh").unwrap(),
        b"#!/bin/sh\necho b\n"
    );
    assert_eq!(mode_of(&repo, "run.sh"), 0o100);
}

#[test]
fn a_binary_file_is_refused_with_a_reason_and_nothing_changes() {
    let repo = TempRepo::new();
    repo.write_bytes("img.bin", b"\x00\x01\x02 old");
    repo.write("a.txt", "a\n");
    repo.commit_all("base");
    repo.write_bytes("img.bin", b"\x00\x01\x02 new");
    repo.write_bytes("fresh.bin", b"\x00\xff");
    let before = repo.all_files();
    for path in ["img.bin", "fresh.bin"] {
        let error =
            load_inputs(&repo.paths, &RepoPath(path.into()), Source::IndexToWorktree).unwrap_err();
        let InputsError::NotText(reason) = error else {
            panic!("{path}: {error:?}");
        };
        assert!(
            reason.contains(path) && reason.contains("binary"),
            "{reason}"
        );
    }
    // A text patch aimed at a binary file by mistake is refused by git, and
    // leaves no undo point behind.
    let mut driver = Driver::open(&repo);
    let based_on = driver.state.fresh_disk().unwrap().index.clone();
    driver.deliver(Event::DiscardRequested(DiscardRequest {
        selection: Selection::File(RepoPath("img.bin".into())),
        patch: Patch(
            "diff --git a/img.bin b/img.bin\n--- a/img.bin\n+++ b/img.bin\n@@ -1 +1 @@\n-old\n+new\n"
                .into(),
        ),
        based_on,
        expect_file: Some(
            shell::staging::blob_id(
                gix_sha1(),
                b"\x00\x01\x02 new",
            )
            .unwrap(),
        ),
    }));
    assert!(
        matches!(driver.last_outcome(), Some(OpOutcome::Failed(_))),
        "{:?}",
        driver.state.notice
    );
    assert_eq!(repo.all_files(), before);
    assert!(driver.state.undo.entries.is_empty());
    assert!(repo.undo_refs().is_empty(), "{:?}", repo.undo_refs());
}

fn gix_sha1() -> gix::hash::Kind {
    gix::hash::Kind::Sha1
}

// ---- refusals -----------------------------------------------------------

#[test]
fn a_patch_built_before_the_index_moved_is_refused_and_changes_nothing() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\n");
    repo.write("g.txt", "g\n");
    repo.commit_all("base");
    repo.write("f.txt", "a\nB\n");
    let mut driver = Driver::open(&repo);
    let request = build_request(&repo.paths, "f.txt", &Choice::File).unwrap();
    // Someone else stages another file in between.
    repo.write("g.txt", "G\n");
    repo.git(&["add", "g.txt"]);
    driver.outside_change();
    driver.deliver(Event::DiscardRequested(request));
    assert_eq!(
        driver.state.notice,
        Some(Notice::Refused {
            action: Action::Discard,
            reason: Refusal::StaleDiff
        })
    );
    assert_eq!(repo.worktree_bytes("f.txt").unwrap(), b"a\nB\n");
    assert!(repo.undo_refs().is_empty());
}

#[test]
fn a_file_edited_after_the_diff_was_read_is_refused_and_kept() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\nc\nd\ne\nf\ng\n");
    repo.commit_all("base");
    repo.write("f.txt", "a\nB\nc\nd\ne\nf\ng\n");
    let mut driver = Driver::open(&repo);
    let request = build_request(&repo.paths, "f.txt", &Choice::File).unwrap();
    // The user keeps typing, far from the selected line: git apply alone
    // would accept the patch, but the file is no longer the one that was on
    // screen.
    repo.write("f.txt", "a\nB\nc\nd\ne\nf\ng\nmore\n");
    driver.deliver(Event::DiscardRequested(request));
    let Some(OpOutcome::Failed(failure)) = driver.last_outcome() else {
        panic!("{:?}", driver.state.notice);
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    assert!(
        failure.details.contains("f.txt changed"),
        "{}",
        failure.details
    );
    assert_eq!(
        repo.worktree_bytes("f.txt").unwrap(),
        b"a\nB\nc\nd\ne\nf\ng\nmore\n"
    );
    assert!(driver.state.undo.entries.is_empty());
    assert!(repo.undo_refs().is_empty(), "{:?}", repo.undo_refs());
}

#[test]
fn a_patch_that_does_not_fit_changes_nothing() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\n");
    repo.commit_all("base");
    repo.write("f.txt", "a\nB\n");
    let mut driver = Driver::open(&repo);
    let mut request = build_request(&repo.paths, "f.txt", &Choice::File).unwrap();
    request.patch = Patch(request.patch.0.replace("-B", "-ZZZ"));
    driver.deliver(Event::DiscardRequested(request));
    let Some(OpOutcome::Failed(failure)) = driver.last_outcome() else {
        panic!("{:?}", driver.state.notice);
    };
    assert!(!failure.details.is_empty());
    assert_eq!(repo.worktree_bytes("f.txt").unwrap(), b"a\nB\n");
    assert!(repo.undo_refs().is_empty());
}

#[test]
fn a_path_outside_the_working_tree_is_refused() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\n");
    repo.commit_all("base");
    fs::write(repo.root.join("outside.txt"), "precious\n").unwrap();
    let mut driver = Driver::open(&repo);
    let based_on = driver.state.fresh_disk().unwrap().index.clone();
    for path in ["../outside.txt", ".git/config", "/etc/passwd", ""] {
        driver.deliver(Event::NoticeDismissed);
        driver.deliver(Event::DiscardRequested(DiscardRequest {
            selection: Selection::File(RepoPath(path.into())),
            patch: Patch("diff --git a/x b/x\ndeleted file mode 100644\n".into()),
            based_on: based_on.clone(),
            expect_file: None,
        }));
        // The shell's own check says why (git would refuse most of these
        // too, but with a message about its index).
        let Some(OpOutcome::Failed(failure)) = driver.last_outcome() else {
            panic!("{path}: {:?}", driver.state.notice);
        };
        assert!(
            failure
                .details
                .contains("is not a path inside the working tree"),
            "{path}: {}",
            failure.details
        );
    }
    assert_eq!(
        fs::read(repo.root.join("outside.txt")).unwrap(),
        b"precious\n"
    );
    assert!(driver.state.undo.entries.is_empty());
}

/// Builds a point the way the core does, for driving the runners by hand.
fn point_for(repo: &TempRepo, op: u64, path: &str) -> invoke_core::UndoPoint {
    let current = shell::precondition::current(&repo.paths).unwrap();
    invoke_core::UndoPoint {
        op: OpId(op),
        label: format!("Discard {path}"),
        head: invoke_core::HeadTarget::Branch(invoke_core::BranchName("main".into())),
        refs: Vec::new(),
        index: current.index,
        index_effect: invoke_core::IndexEffect::Untouched,
        reverse: invoke_core::Reversal::Discard {
            path: RepoPath(path.into()),
        },
    }
}

/// The runner checks the file's bytes and the saved copy itself, one check
/// each: the core cannot see the file, and the file can change between the
/// snapshot and the patch.
#[test]
fn the_runner_checks_the_file_and_its_saved_copy_itself() {
    let git = || Git::detect("git").unwrap();
    let blob = |bytes: &[u8]| shell::staging::blob_id(gix::hash::Kind::Sha1, bytes).unwrap();
    let path = RepoPath("f.txt".into());
    let patch = Patch(
        "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n a\n-B\n+b\n"
            .into(),
    );
    let setup = || {
        let repo = TempRepo::new();
        repo.write("f.txt", "a\nb\n");
        repo.commit_all("base");
        repo.write("f.txt", "a\nB\n");
        repo
    };
    let runners = |repo: &TempRepo| {
        (
            UndoRunner::new(git(), repo.paths.clone()).with_env(repo.env.clone()),
            DiscardRunner::new(git(), repo.paths.clone()).with_env(repo.env.clone()),
        )
    };
    let failure_of = |event: Event| match event {
        Event::OpFinished(_, OpOutcome::Failed(failure)) => failure,
        other => panic!("expected a refusal, got {other:?}"),
    };

    // 1. The file changed after the snapshot: the file check refuses, the
    //    saved copy still matches what the request expected.
    let repo = setup();
    let (undo, discard) = runners(&repo);
    let point = point_for(&repo, 7, "f.txt");
    assert!(matches!(undo.snapshot(&point), Snapshotted::Taken(_)));
    repo.write("f.txt", "a\nB\nmore\n");
    let index = shell::precondition::current(&repo.paths).unwrap().index;
    let event = discard.apply(OpId(7), &patch, &path, &index, Some(&blob(b"a\nB\n")));
    let failure = failure_of(event);
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    assert!(failure.details.contains("f.txt changed"), "{failure:?}");
    assert_eq!(repo.worktree_bytes("f.txt").unwrap(), b"a\nB\nmore\n");

    // 2. The request expects the file as it is now, but the snapshot saved
    //    an older version: undo would bring back the wrong content, so the
    //    saved-copy check refuses.
    let repo = setup();
    let (undo, discard) = runners(&repo);
    let point = point_for(&repo, 8, "f.txt");
    assert!(matches!(undo.snapshot(&point), Snapshotted::Taken(_)));
    repo.write("f.txt", "a\nB\nmore\n");
    let index = shell::precondition::current(&repo.paths).unwrap().index;
    let event = discard.apply(OpId(8), &patch, &path, &index, Some(&blob(b"a\nB\nmore\n")));
    let failure = failure_of(event);
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    assert!(
        failure.details.contains("while it was saved for undo"),
        "{failure:?}"
    );
    assert_eq!(repo.worktree_bytes("f.txt").unwrap(), b"a\nB\nmore\n");

    // 3. Both agree: it applies.
    let repo = setup();
    let (undo, discard) = runners(&repo);
    let point = point_for(&repo, 9, "f.txt");
    assert!(matches!(undo.snapshot(&point), Snapshotted::Taken(_)));
    let index = shell::precondition::current(&repo.paths).unwrap().index;
    let event = discard.apply(OpId(9), &patch, &path, &index, Some(&blob(b"a\nB\n")));
    assert_eq!(event, Event::OpFinished(OpId(9), OpOutcome::Succeeded));
    assert_eq!(repo.worktree_bytes("f.txt").unwrap(), b"a\nb\n");
}

#[test]
fn undo_removes_the_folder_it_empties() {
    let repo = TempRepo::new();
    repo.write("gone dir/sub/x.txt", "x\n");
    repo.write("keep.txt", "k\n");
    repo.commit_all("base");
    fs::remove_dir_all(repo.paths.workdir.join("gone dir")).unwrap();
    let mut driver = Driver::open(&repo);
    assert!(driver.discard("gone dir/sub/x.txt", &Choice::File));
    assert!(repo.exists("gone dir/sub/x.txt"));
    driver.undo();
    assert!(!repo.exists("gone dir"), "an empty folder was left behind");
    assert!(repo.exists("keep.txt"));
}

#[test]
fn a_discard_is_refused_in_the_middle_of_a_merge() {
    let repo = TempRepo::new();
    repo.write("f.txt", "base\n");
    repo.write("other.txt", "o\n");
    repo.commit_all("base");
    repo.git(&["switch", "-q", "-c", "side"]);
    repo.write("f.txt", "side\n");
    repo.commit_all("side");
    repo.git(&["switch", "-q", "main"]);
    repo.write("f.txt", "main\n");
    repo.commit_all("main");
    let merge = repo.output(&["merge", "side"]);
    assert!(!merge.status.success(), "the merge must conflict");
    repo.write("other.txt", "edited\n");
    let mut driver = Driver::open(&repo);
    let request = build_request(&repo.paths, "other.txt", &Choice::File).unwrap();
    driver.deliver(Event::DiscardRequested(request));
    assert_eq!(
        driver.state.notice,
        Some(Notice::Refused {
            action: Action::Discard,
            reason: Refusal::RepoBusy
        })
    );
    assert_eq!(repo.worktree_bytes("other.txt").unwrap(), b"edited\n");
}

// ---- undo ---------------------------------------------------------------

#[test]
fn undo_does_not_overwrite_a_file_changed_after_the_discard() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\n");
    repo.commit_all("base");
    repo.write("f.txt", "a\nB\n");
    let mut driver = Driver::open(&repo);
    assert!(driver.discard("f.txt", &Choice::File));
    // The user edits the file again.
    repo.write("f.txt", "a\nb\nnew work\n");
    driver.outside_change();
    driver.deliver(Event::UndoRequested);
    let Some(OpOutcome::Failed(failure)) = driver.last_outcome() else {
        panic!("{:?}", driver.state.notice);
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
    assert_eq!(repo.worktree_bytes("f.txt").unwrap(), b"a\nb\nnew work\n");
    // The point stays, and the saved content is still kept alive.
    assert_eq!(driver.state.undo.entries.len(), 1);
    assert_eq!(repo.undo_refs().len(), 3, "{:?}", repo.undo_refs());
    // Put the file back to what the discard left and the undo goes through.
    repo.write("f.txt", "a\nb\n");
    driver.undo();
    assert_eq!(repo.worktree_bytes("f.txt").unwrap(), b"a\nB\n");
}

#[test]
fn chained_discards_undo_in_order() {
    let repo = TempRepo::new();
    repo.write("a.txt", "1\n2\n");
    repo.write("b.txt", "x\ny\n");
    repo.commit_all("base");
    repo.write("a.txt", "1\nTWO\n");
    repo.write("b.txt", "X\ny\n");
    let mut driver = Driver::open(&repo);
    assert!(driver.discard("a.txt", &Choice::File));
    driver.outside_change();
    assert!(driver.discard("b.txt", &Choice::File));
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
    driver.undo();
    assert_eq!(repo.worktree_bytes("b.txt").unwrap(), b"X\ny\n");
    assert_eq!(repo.worktree_bytes("a.txt").unwrap(), b"1\n2\n");
    driver.undo();
    assert_eq!(repo.worktree_bytes("a.txt").unwrap(), b"1\nTWO\n");
}

#[test]
fn saved_content_survives_garbage_collection() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\n");
    repo.commit_all("base");
    let unique = "only in the worktree, never committed or staged\n";
    repo.write("f.txt", &format!("a\nb\n{unique}"));
    let mut driver = Driver::open(&repo);
    assert!(driver.discard("f.txt", &Choice::File));
    repo.git(&["reflog", "expire", "--expire=now", "--all"]);
    repo.git(&["gc", "-q", "--prune=now"]);
    driver.undo();
    assert_eq!(
        repo.worktree_bytes("f.txt").unwrap(),
        format!("a\nb\n{unique}").as_bytes()
    );
}

#[test]
fn a_discard_never_touches_the_index_file() {
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\n");
    repo.write("staged.txt", "s\n");
    repo.commit_all("base");
    repo.write("staged.txt", "S\n");
    repo.git(&["add", "staged.txt"]);
    repo.write("f.txt", "a\nB\n");
    let index_file = repo.index_file_bytes();
    let mut driver = Driver::open(&repo);
    assert!(driver.discard("f.txt", &Choice::File));
    assert_eq!(repo.index_file_bytes(), index_file);
    driver.undo();
    assert_eq!(repo.index_file_bytes(), index_file);
    assert_eq!(repo.git(&["diff", "--cached", "--name-only"]), "staged.txt");
}

#[test]
fn the_index_id_is_unchanged_so_the_diff_is_still_current_after_a_discard() {
    // A discard leaves the index alone, so a second request built from the
    // same read still passes the stale-index check; the file check is what
    // protects the second request.
    let repo = TempRepo::new();
    repo.write("f.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n");
    repo.commit_all("base");
    repo.write("f.txt", "A\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nL\n");
    let mut driver = Driver::open(&repo);
    let both = build_request(&repo.paths, "f.txt", &Choice::Hunk(1)).unwrap();
    assert!(driver.discard("f.txt", &Choice::Hunk(0)));
    let IndexId::Checksum(_) = &both.based_on else {
        panic!("the index must exist");
    };
    driver.deliver(Event::DiscardRequested(both));
    let Some(OpOutcome::Failed(failure)) = driver.last_outcome() else {
        panic!("{:?}", driver.state.notice);
    };
    assert_eq!(failure.class, ErrorClass::PreconditionChanged);
}

// ---- core.autocrlf (task 13 UI: Git for Windows' default) ----------------

/// With `core.autocrlf=true` the index holds LF and the worktree CRLF. The
/// diff shown (and so the patch) is made from the filtered text; the discard
/// must still change only the selected lines and keep the file's CRLF.
#[test]
fn a_discard_under_autocrlf_keeps_the_crlf_worktree() {
    let repo = TempRepo::new();
    repo.git(&["config", "core.autocrlf", "true"]);
    let base: String = (0..30).map(|i| format!("line {i}\r\n")).collect();
    repo.write("w.txt", &base);
    repo.commit_all("base");
    let eol = repo.git(&["ls-files", "--eol", "w.txt"]);
    assert!(eol.contains("i/lf") && eol.contains("w/crlf"), "{eol}");
    let edited = base
        .replace("line 2\r\n", "LINE 2\r\n")
        .replace("line 25\r\n", "line 25\r\nextra\r\n");
    repo.write("w.txt", &edited);
    let mut driver = Driver::open(&repo);
    assert!(driver.discard("w.txt", &Choice::Hunk(1)));
    let expected = base.replace("line 2\r\n", "LINE 2\r\n");
    assert_eq!(repo.worktree_bytes("w.txt").unwrap(), expected.as_bytes());
    driver.undo();
    assert_eq!(repo.worktree_bytes("w.txt").unwrap(), edited.as_bytes());
    // The whole file, too.
    assert!(driver.discard("w.txt", &Choice::File));
    assert_eq!(repo.worktree_bytes("w.txt").unwrap(), base.as_bytes());
}
