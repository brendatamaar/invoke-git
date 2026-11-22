//! `core.autocrlf` (task 11, spec question 4): Git for Windows' default is
//! `core.autocrlf=true`, so a file is `LF` in the index and `CRLF` in the
//! worktree. A status read and a diff that ignore the setting would show
//! every such file as changed on every line.
//!
//! The setting is written to the repo's own config in these tests, and the
//! user's and the system's config are shut out, so the tests do not depend
//! on the machine. `tests/user_config.rs` covers the setting in the user's
//! own config file.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use diff::{DiffOptions, Direction, Mode, PatchSpec, Pick, build_patch, diff_text};
use invoke_core::{FileChange, RepoPath};
use shell::precondition::RepoPaths;
use shell::{Source, load_inputs, read::read_state};

struct Fixture {
    root: PathBuf,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
}

impl Fixture {
    /// A repo with `core.autocrlf` set to `setting` in its own config
    /// (`None` leaves it unset), and a global config file with `global`.
    fn new(setting: Option<&str>, global: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "invoke-git-autocrlf-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let workdir = root.join("repo");
        fs::create_dir_all(&workdir).unwrap();
        let global_file = root.join("gitconfig");
        fs::write(&global_file, global).unwrap();
        let env = [
            ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
            ("GIT_CONFIG_GLOBAL", global_file.into_os_string()),
            ("GIT_AUTHOR_NAME", "Test".into()),
            ("GIT_AUTHOR_EMAIL", "test@example.com".into()),
            ("GIT_COMMITTER_NAME", "Test".into()),
            ("GIT_COMMITTER_EMAIL", "test@example.com".into()),
        ]
        .into_iter()
        .map(|(key, value)| (OsString::from(key), value))
        .collect();
        let fixture = Self {
            paths: RepoPaths {
                git_dir: workdir.join(".git"),
                workdir,
            },
            root,
            env,
        };
        fixture.git(&["init", "-q", "-b", "main"]);
        if let Some(setting) = setting {
            fixture.git(&["config", "core.autocrlf", setting]);
        }
        fixture
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        fs::write(self.paths.workdir.join(path), bytes).unwrap();
    }

    fn commit_all(&self) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "c"]);
    }

    fn status(&self) -> Vec<(String, Option<FileChange>, Option<FileChange>)> {
        read_state(&self.paths)
            .unwrap()
            .status
            .entries
            .into_iter()
            .map(|e| (e.path.0, e.staged, e.unstaged))
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn crlf(text: &str) -> Vec<u8> {
    text.replace('\n', "\r\n").into_bytes()
}

/// A committed file whose worktree copy has `CRLF` endings (what Git for
/// Windows checks out with `autocrlf=true`) is not changed.
fn committed_with_crlf_worktree(fixture: &Fixture) {
    fixture.write("a.txt", &crlf("one\ntwo\nthree\n"));
    fixture.commit_all();
    // git add stored LF; rewrite the worktree copy as a checkout would.
    fixture.write("a.txt", &crlf("one\ntwo\nthree\n"));
}

#[test]
fn repo_setting_a_crlf_worktree_file_is_not_a_change() {
    let fixture = Fixture::new(Some("true"), "");
    committed_with_crlf_worktree(&fixture);
    assert_eq!(fixture.git(&["status", "--porcelain"]), "");
    assert_eq!(fixture.status(), vec![], "status must agree with git");
}

#[test]
fn repo_setting_the_diff_shows_only_the_edited_line() {
    let fixture = Fixture::new(Some("true"), "");
    committed_with_crlf_worktree(&fixture);
    fixture.write("a.txt", &crlf("one\nTWO\nthree\n"));
    let inputs = load_inputs(
        &fixture.paths,
        &RepoPath("a.txt".into()),
        Source::IndexToWorktree,
    )
    .unwrap();
    let (old, new) = (inputs.old.unwrap(), inputs.new.unwrap());
    // The new side is what `git add` would store: LF, like the index.
    assert_eq!(new, "one\nTWO\nthree\n");
    assert_eq!(old, "one\ntwo\nthree\n");
}

#[test]
fn repo_setting_staging_a_line_keeps_the_index_lf() {
    let fixture = Fixture::new(Some("true"), "");
    committed_with_crlf_worktree(&fixture);
    fixture.write("a.txt", &crlf("one\nTWO\nthree\nfour\n"));
    let path = RepoPath("a.txt".into());
    let inputs = load_inputs(&fixture.paths, &path, Source::IndexToWorktree).unwrap();
    let text = diff_text(
        inputs.old.clone().unwrap(),
        inputs.new.clone().unwrap(),
        &DiffOptions::default(),
    );
    let spec = PatchSpec {
        path: "a.txt",
        direction: Direction::Stage,
        old_exists: true,
        new_exists: true,
        mode: Mode::Regular,
    };
    let patch = build_patch(&text, &spec, &Pick::File).unwrap().unwrap();
    assert!(!patch.contains('\r'), "an LF index takes an LF patch");
    assert!(patch.contains("-two") && patch.contains("+TWO") && patch.contains("+four"));
    assert!(
        !patch.contains("-one"),
        "no line may differ only in its ending"
    );
}

#[test]
fn autocrlf_false_keeps_crlf_as_a_real_change() {
    let fixture = Fixture::new(Some("false"), "");
    fixture.write("a.txt", b"one\ntwo\n");
    fixture.commit_all();
    fixture.write("a.txt", &crlf("one\ntwo\n"));
    assert_eq!(fixture.git(&["status", "--porcelain"]), "M a.txt");
    assert_eq!(
        fixture.status(),
        vec![("a.txt".to_owned(), None, Some(FileChange::Modified))]
    );
}

#[test]
fn repo_setting_real_git_stages_the_line_into_an_lf_index() {
    use invoke_core::{Event, OpId, OpOutcome, Patch};
    use shell::{Git, StagingRunner};

    let fixture = Fixture::new(Some("true"), "");
    committed_with_crlf_worktree(&fixture);
    fixture.write("a.txt", &crlf("one\nTWO\nthree\nfour\n"));
    let path = RepoPath("a.txt".into());
    let inputs = load_inputs(&fixture.paths, &path, Source::IndexToWorktree).unwrap();
    let text = diff_text(
        inputs.old.clone().unwrap(),
        inputs.new.clone().unwrap(),
        &DiffOptions::default(),
    );
    let spec = PatchSpec {
        path: "a.txt",
        direction: Direction::Stage,
        old_exists: true,
        new_exists: true,
        mode: Mode::Regular,
    };
    let patch = build_patch(&text, &spec, &Pick::File).unwrap().unwrap();
    let runner = StagingRunner::new(Git::detect("git").unwrap(), fixture.paths.clone())
        .with_env(fixture.env.clone());
    match runner.apply(OpId(1), &Patch(patch), &inputs.based_on) {
        Event::OpFinished(_, OpOutcome::Succeeded) => {}
        other => panic!("staging failed: {other:?}"),
    }
    // What `git add a.txt` would have stored.
    let staged = fixture.git(&["cat-file", "blob", ":a.txt"]);
    assert_eq!(staged, "one\nTWO\nthree\nfour");
    // And nothing is left over: the worktree file equals the index again.
    assert_eq!(fixture.git(&["status", "--porcelain"]), "M  a.txt");
}

/// The worktree side's blob id is that of the file's own bytes (CRLF and
/// all): discard and undo compare exactly those.
#[test]
fn repo_setting_the_new_blob_is_the_raw_file() {
    let fixture = Fixture::new(Some("true"), "");
    committed_with_crlf_worktree(&fixture);
    let raw = crlf("one\nTWO\nthree\n");
    fixture.write("a.txt", &raw);
    let inputs = load_inputs(
        &fixture.paths,
        &RepoPath("a.txt".into()),
        Source::IndexToWorktree,
    )
    .unwrap();
    let expected = fixture.git(&["hash-object", "--no-filters", "a.txt"]);
    assert_eq!(inputs.new_blob.unwrap().as_hex(), expected);
}
