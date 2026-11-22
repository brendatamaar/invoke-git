//! What a commit changed (task 19), on generated repos with real git as the
//! oracle. System and global git config are shut out.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use invoke_core::RepoPath;
use proptest::prelude::*;
use shell::InputsError;
use shell::commit_detail::{Change, read_commit, read_commit_sides};
use shell::history::CommitId;
use shell::precondition::RepoPaths;

struct Repo {
    root: PathBuf,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
}

impl Repo {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "invoke-git-detail-{}-{}",
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
            ("GIT_AUTHOR_NAME", "Ada Lovelace".into()),
            ("GIT_AUTHOR_EMAIL", "ada@example.com".into()),
            ("GIT_AUTHOR_DATE", "2024-03-05T10:20:30+0700".into()),
            ("GIT_COMMITTER_NAME", "Ada Lovelace".into()),
            ("GIT_COMMITTER_EMAIL", "ada@example.com".into()),
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

    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
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

    fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.paths.workdir, args)
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.paths.workdir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn remove(&self, name: &str) {
        fs::remove_file(self.paths.workdir.join(name)).unwrap();
    }

    fn commit(&self, message: &str) -> CommitId {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.head()
    }

    fn head(&self) -> CommitId {
        CommitId::from_hex(self.git(&["rev-parse", "HEAD"]).as_bytes()).unwrap()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// `git diff-tree` (no renames) as `(letter, path)`, sorted by path.
fn oracle(repo: &Repo, id: &CommitId) -> Vec<(String, String)> {
    let hex = id.to_string();
    let text = repo.git(&[
        "diff-tree",
        "-r",
        "--root",
        "--no-renames",
        "--no-commit-id",
        "--name-status",
        &hex,
    ]);
    let mut rows: Vec<(String, String)> = text
        .lines()
        .map(|line| {
            let (letter, path) = line.split_once('\t').unwrap();
            (letter.to_owned(), path.to_owned())
        })
        .collect();
    rows.sort_by(|a, b| a.1.cmp(&b.1));
    rows
}

fn letter(change: Change) -> &'static str {
    match change {
        Change::Added => "A",
        Change::Modified => "M",
        Change::Deleted => "D",
        Change::TypeChanged => "T",
    }
}

fn listed(repo: &Repo, id: &CommitId) -> Vec<(String, String)> {
    read_commit(&repo.paths, id)
        .unwrap()
        .files
        .into_iter()
        .map(|file| (letter(file.change).to_owned(), file.path))
        .collect()
}

#[test]
fn a_root_commit_adds_every_file_and_has_no_parent() {
    let repo = Repo::new();
    repo.write("a.txt", "one\n");
    repo.write("src/deep/b.txt", "two\n");
    let id = repo.commit("first\n\nwith a body\nover two lines");
    let detail = read_commit(&repo.paths, &id).unwrap();
    assert!(detail.parents.is_empty());
    assert!(!detail.is_merge());
    assert_eq!(detail.id, id.to_string());
    assert_eq!(detail.author, "Ada Lovelace");
    assert_eq!(detail.email, "ada@example.com");
    // 2024-03-05 10:20:30 at +07:00.
    assert_eq!(detail.seconds, 1_709_608_830);
    assert_eq!(detail.offset, 7 * 3600);
    assert_eq!(
        detail.message.trim_end(),
        "first\n\nwith a body\nover two lines"
    );
    assert_eq!(
        listed(&repo, &id),
        vec![
            ("A".to_owned(), "a.txt".to_owned()),
            ("A".to_owned(), "src/deep/b.txt".to_owned())
        ]
    );
}

#[test]
fn a_commit_lists_additions_changes_and_deletions_but_not_folders() {
    let repo = Repo::new();
    repo.write("keep.txt", "k\n");
    repo.write("edit.txt", "old\n");
    repo.write("gone/x.txt", "x\n");
    repo.commit("base");
    repo.write("edit.txt", "new\n");
    repo.remove("gone/x.txt");
    repo.write("fresh/dir/y.txt", "y\n");
    let id = repo.commit("change things");
    assert_eq!(
        listed(&repo, &id),
        vec![
            ("M".to_owned(), "edit.txt".to_owned()),
            ("A".to_owned(), "fresh/dir/y.txt".to_owned()),
            ("D".to_owned(), "gone/x.txt".to_owned()),
        ]
    );
    assert_eq!(listed(&repo, &id), oracle(&repo, &id));
}

#[test]
fn an_empty_commit_changes_no_file() {
    let repo = Repo::new();
    repo.write("a", "a\n");
    repo.commit("base");
    let id = repo.commit("nothing");
    assert!(read_commit(&repo.paths, &id).unwrap().files.is_empty());
}

#[test]
fn a_merge_is_shown_against_its_first_parent() {
    let repo = Repo::new();
    repo.write("a", "a\n");
    repo.commit("base");
    repo.git(&["switch", "-q", "-c", "side"]);
    repo.write("side.txt", "s\n");
    repo.commit("side");
    repo.git(&["switch", "-q", "main"]);
    repo.write("main.txt", "m\n");
    repo.commit("main");
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
    let id = repo.head();
    let detail = read_commit(&repo.paths, &id).unwrap();
    assert!(detail.is_merge());
    assert_eq!(detail.parents.len(), 2);
    // Against the first parent (main), the merge brought in `side.txt`.
    assert_eq!(
        detail
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        vec!["side.txt"]
    );
    assert_eq!(
        detail.parents[0],
        repo.git(&["rev-parse", "HEAD^1"]),
        "parents keep their order"
    );
}

#[test]
fn the_sides_of_a_file_are_its_text_before_and_after() {
    let repo = Repo::new();
    repo.write("a.txt", "one\ntwo\n");
    let first = repo.commit("first");
    repo.write("a.txt", "one\nthree\n");
    repo.write("b.txt", "new\n");
    let second = repo.commit("second");
    let path = |p: &str| RepoPath(p.to_owned());

    let sides = read_commit_sides(&repo.paths, &second, &path("a.txt")).unwrap();
    assert_eq!(sides.old.as_deref(), Some("one\ntwo\n"));
    assert_eq!(sides.new.as_deref(), Some("one\nthree\n"));

    let added = read_commit_sides(&repo.paths, &second, &path("b.txt")).unwrap();
    assert_eq!((added.old, added.new.as_deref()), (None, Some("new\n")));

    // In a root commit the old side is empty.
    let root = read_commit_sides(&repo.paths, &first, &path("a.txt")).unwrap();
    assert_eq!((root.old, root.new.as_deref()), (None, Some("one\ntwo\n")));
}

#[test]
fn a_deleted_file_has_no_new_side() {
    let repo = Repo::new();
    repo.write("a.txt", "bye\n");
    repo.commit("first");
    repo.remove("a.txt");
    let id = repo.commit("remove");
    let sides = read_commit_sides(&repo.paths, &id, &RepoPath("a.txt".into())).unwrap();
    assert_eq!((sides.old.as_deref(), sides.new), (Some("bye\n"), None));
}

#[test]
fn binary_and_non_utf8_files_have_no_text_diff() {
    let repo = Repo::new();
    fs::write(repo.paths.workdir.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
    fs::write(repo.paths.workdir.join("latin.txt"), [0xE9u8, b'\n']).unwrap();
    let id = repo.commit("binary");
    for name in ["bin.dat", "latin.txt"] {
        let error = read_commit_sides(&repo.paths, &id, &RepoPath(name.into())).unwrap_err();
        assert!(
            matches!(error, InputsError::NotText(_)),
            "{name}: {error:?}"
        );
    }
    // They are still listed as changed files.
    assert_eq!(read_commit(&repo.paths, &id).unwrap().files.len(), 2);
}

#[test]
fn an_unknown_commit_is_an_error_not_a_panic() {
    let repo = Repo::new();
    repo.write("a", "a\n");
    repo.commit("base");
    let missing = CommitId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
    assert!(read_commit(&repo.paths, &missing).is_err());
    assert!(read_commit_sides(&repo.paths, &missing, &RepoPath("a".into())).is_err());
}

#[test]
fn unicode_and_spaced_paths_survive() {
    let repo = Repo::new();
    repo.write("dir with space/naïve café.txt", "x\n");
    let id = repo.commit("unicode");
    let detail = read_commit(&repo.paths, &id).unwrap();
    assert_eq!(detail.files[0].path, "dir with space/naïve café.txt");
    let sides = read_commit_sides(
        &repo.paths,
        &id,
        &RepoPath("dir with space/naïve café.txt".into()),
    )
    .unwrap();
    assert_eq!(sides.new.as_deref(), Some("x\n"));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    /// The listing of every commit of a random history agrees with
    /// `git diff-tree`, and each listed text file reads back both sides.
    #[test]
    fn the_listing_agrees_with_git(
        steps in prop::collection::vec(
            (prop::collection::vec((0usize..6, 0u8..3), 1..4), any::<bool>()),
            1..6,
        )
    ) {
        let repo = Repo::new();
        let names = ["a.txt", "b.txt", "d1/c.txt", "d1/d2/e.txt", "d3/f.txt", "g.txt"];
        let mut counter = 0u32;
        let mut ids = Vec::new();
        for (edits, _) in &steps {
            for (which, action) in edits {
                let name = names[*which];
                let exists = repo.paths.workdir.join(name).exists();
                counter += 1;
                match (action, exists) {
                    (0, true) => repo.remove(name),
                    _ => repo.write(name, &format!("line {counter}\nsame\n")),
                }
            }
            ids.push(repo.commit(&format!("step {counter}")));
        }
        for id in &ids {
            prop_assert_eq!(listed(&repo, id), oracle(&repo, id));
            let detail = read_commit(&repo.paths, id).unwrap();
            for file in &detail.files {
                let sides = read_commit_sides(&repo.paths, id, &RepoPath(file.path.clone())).unwrap();
                match file.change {
                    Change::Added => prop_assert!(sides.old.is_none() && sides.new.is_some()),
                    Change::Deleted => prop_assert!(sides.old.is_some() && sides.new.is_none()),
                    _ => prop_assert!(sides.old.is_some() && sides.new.is_some()),
                }
            }
        }
    }
}
