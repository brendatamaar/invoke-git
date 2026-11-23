use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use invoke_core::search::{SearchField, SearchQuery};
use shell::precondition::RepoPaths;
use shell::search::{SCAN_PAGE_SIZE, SearchEvent, search};

const TIMEOUT: Duration = Duration::from_secs(10);

struct Repo {
    root: PathBuf,
    paths: RepoPaths,
}

impl Repo {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "invoke-git-search-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workdir = root.join("repo");
        fs::create_dir_all(&workdir).unwrap();
        fs::write(root.join("gitconfig"), "").unwrap();
        let repo = Self {
            root,
            paths: RepoPaths {
                git_dir: workdir.join(".git"),
                workdir,
            },
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.paths.workdir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
            .env("GIT_AUTHOR_NAME", "Ada Lovelace")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com");
        cmd
    }

    fn git(&self, args: &[&str]) -> String {
        let result = self.command().args(args).output().unwrap();
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap().trim().to_owned()
    }

    fn commit(&self, args: &[&str]) -> String {
        self.git(&["add", "-A"]);
        self.git(&[&["commit", "-q"][..], args].concat());
        self.git(&["rev-parse", "HEAD"])
    }

    fn matches(&self, field: SearchField, value: &str) -> Vec<String> {
        let rx = search(
            self.paths.clone(),
            SearchQuery::parse(field, value).unwrap(),
            Arc::new(AtomicBool::new(false)),
        );
        let mut ids = Vec::new();
        loop {
            match rx.recv_timeout(TIMEOUT).unwrap() {
                SearchEvent::Page { matches, scanned } => {
                    assert!(matches.len() <= scanned && scanned <= SCAN_PAGE_SIZE);
                    ids.extend(matches.iter().map(ToString::to_string));
                }
                SearchEvent::Done => return ids,
                SearchEvent::Failed(error) => panic!("{error}"),
            }
        }
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn searches_commit_metadata_and_changed_paths() {
    let repo = Repo::new();
    let path = "dir/exact file.txt";
    fs::create_dir(repo.paths.workdir.join("dir")).unwrap();
    fs::write(repo.paths.workdir.join(path), "first").unwrap();
    let root = repo.commit(&["-m", "initial", "-m", "Needle in the body"]);
    repo.git(&["branch", "side"]);
    fs::write(repo.paths.workdir.join("other.txt"), "other").unwrap();
    let kept = repo.commit(&["-m", "kept", "--author", "Grace Hopper <navy@example.com>"]);
    fs::remove_file(repo.paths.workdir.join(path)).unwrap();
    let deleted = repo.commit(&["-m", "removed"]);
    repo.git(&["switch", "side"]);
    fs::write(repo.paths.workdir.join(path), "second").unwrap();
    let modified = repo.commit(&["-m", "side branch"]);
    repo.git(&["switch", "main"]);
    repo.git(&["commit-graph", "write", "--reachable"]);

    assert_eq!(
        repo.matches(SearchField::Message, "NEEDLE"),
        [root.as_str()]
    );
    assert_eq!(repo.matches(SearchField::Author, "HOPPER"), [kept.as_str()]);
    assert_eq!(repo.matches(SearchField::Author, "navy@"), [kept.as_str()]);
    assert_eq!(repo.matches(SearchField::Hash, &kept), [kept]);
    assert_eq!(
        repo.matches(SearchField::Hash, &modified[..12].to_uppercase()),
        [modified.as_str()]
    );
    let mut touched = repo.matches(SearchField::Path, path);
    touched.sort();
    let mut expected = [root, deleted, modified];
    expected.sort();
    assert_eq!(touched, expected);
    assert!(
        repo.matches(SearchField::Path, "dir/Exact file.txt")
            .is_empty()
    );
}

#[test]
fn pages_progress_and_cancels_before_finishing() {
    let repo = Repo::new();
    let mut child = repo
        .command()
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let count = 4 * SCAN_PAGE_SIZE + 1;
    for i in 0..count {
        write!(
            input,
            "commit refs/heads/main\ncommitter Ada <ada@example.com> {} +0000\ndata 4\nscan\n\n",
            1_700_000_000 + i
        )
        .unwrap();
    }
    drop(input);
    assert!(child.wait().unwrap().success());

    assert_eq!(repo.matches(SearchField::Message, "scan").len(), count);
    let cancel = Arc::new(AtomicBool::new(false));
    let rx = search(
        repo.paths.clone(),
        SearchQuery::parse(SearchField::Message, "absent").unwrap(),
        cancel.clone(),
    );
    assert!(matches!(
        rx.recv_timeout(TIMEOUT).unwrap(),
        SearchEvent::Page { matches, scanned: SCAN_PAGE_SIZE } if matches.is_empty()
    ));
    cancel.store(true, Ordering::Relaxed);
    while let Ok(event) = rx.recv_timeout(TIMEOUT) {
        assert!(matches!(event, SearchEvent::Page { .. }), "{event:?}");
    }
    assert_eq!(
        rx.try_recv().unwrap_err(),
        std::sync::mpsc::TryRecvError::Disconnected
    );
}

#[test]
fn shallow_boundary_has_metadata_but_no_known_path_change() {
    let repo = Repo::new();
    fs::write(repo.paths.workdir.join("file"), "first").unwrap();
    repo.commit(&["-m", "root"]);
    fs::write(repo.paths.workdir.join("file"), "second").unwrap();
    let boundary = repo.commit(&["-m", "shallow boundary"]);
    fs::write(repo.paths.git_dir.join("shallow"), format!("{boundary}\n")).unwrap();

    assert_eq!(repo.matches(SearchField::Message, "shallow"), [boundary]);
    assert!(repo.matches(SearchField::Message, "root").is_empty());
    assert!(repo.matches(SearchField::Path, "file").is_empty());
}

#[test]
fn shallow_boundary_does_not_hide_history_reachable_from_another_tip() {
    let repo = Repo::new();
    let root = repo.commit(&["--allow-empty", "-m", "shared root"]);
    repo.git(&["branch", "side"]);
    let boundary = repo.commit(&["--allow-empty", "-m", "boundary"]);
    repo.git(&["switch", "side"]);
    repo.commit(&["--allow-empty", "-m", "side tip"]);
    fs::write(repo.paths.git_dir.join("shallow"), format!("{boundary}\n")).unwrap();

    assert_eq!(repo.matches(SearchField::Message, "shared root"), [root]);
}
