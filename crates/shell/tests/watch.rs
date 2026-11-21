//! The watcher against real files in generated temp repos. Timeouts are
//! generous: macOS FSEvents can lag, and CI machines are slow.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use invoke_core::{Area, Event};
use shell::{RepoWatcher, discover};

/// How long we wait for an event that must come.
const PATIENCE: Duration = Duration::from_secs(20);
/// How long silence must last before we call it silence.
const SETTLE: Duration = Duration::from_millis(700);

struct TempRepo {
    root: PathBuf,
    dir: PathBuf,
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
            "invoke-git-watch-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let dir = root.join("repo");
        fs::create_dir_all(&dir).unwrap();
        // Canonical: macOS temp folders are symlinks, and events carry the
        // real path. Not on Windows: canonical paths there are `\\?\` paths,
        // which Git for Windows rejects in `GIT_CONFIG_GLOBAL`.
        #[cfg(not(windows))]
        let root = fs::canonicalize(&root).unwrap();
        let dir = root.join("repo");
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
        let repo = Self { root, dir, env };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write(&self, path: &str, text: &str) {
        let full = self.dir.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
    }

    fn watch(&self) -> (RepoWatcher, Receiver<Event>) {
        let found = discover(&self.dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = RepoWatcher::start(&found, tx).unwrap();
        (watcher, rx)
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Waits for the first event, then collects until [`SETTLE`] of silence.
/// Returns each event's area set, in order.
fn collect(rx: &Receiver<Event>) -> Vec<BTreeSet<Area>> {
    let mut out = Vec::new();
    let deadline = Instant::now() + PATIENCE;
    while out.is_empty() && Instant::now() < deadline {
        if let Ok(event) = rx.recv_timeout(Duration::from_millis(100)) {
            out.push(areas(event));
        }
    }
    while let Ok(event) = rx.recv_timeout(SETTLE) {
        out.push(areas(event));
    }
    out
}

fn areas(event: Event) -> BTreeSet<Area> {
    match event {
        Event::DiskChanged(set) => set.0,
        other => panic!("unexpected event {other:?}"),
    }
}

fn silent(rx: &Receiver<Event>) {
    if let Ok(event) = rx.recv_timeout(SETTLE) {
        panic!("expected no event, got {event:?}");
    }
}

fn union(events: &[BTreeSet<Area>]) -> BTreeSet<Area> {
    events.iter().flatten().copied().collect()
}

fn seeded() -> TempRepo {
    let repo = TempRepo::new();
    repo.write("a.txt", "one\n");
    repo.write(".gitignore", "target/\n*.log\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-q", "-m", "first"]);
    repo
}

#[test]
fn worktree_write_gives_one_worktree_event() {
    let repo = seeded();
    let (_watcher, rx) = repo.watch();
    repo.write("a.txt", "two\n");
    let events = collect(&rx);
    assert_eq!(events, vec![BTreeSet::from([Area::Worktree])]);
}

#[test]
fn commit_reports_refs_and_index() {
    let repo = seeded();
    repo.write("b.txt", "b\n");
    repo.git(&["add", "b.txt"]);
    let (_watcher, rx) = repo.watch();
    repo.git(&["commit", "-q", "-m", "second"]);
    let all = union(&collect(&rx));
    assert!(all.contains(&Area::Refs), "{all:?}");
    assert!(all.contains(&Area::Index), "{all:?}");
    assert!(!all.contains(&Area::Worktree), "{all:?}");
}

#[test]
fn switching_branch_reports_head() {
    let repo = seeded();
    let (_watcher, rx) = repo.watch();
    repo.git(&["checkout", "-q", "-b", "topic"]);
    let all = union(&collect(&rx));
    assert!(all.contains(&Area::Head), "{all:?}");
    assert!(all.contains(&Area::Refs), "{all:?}");
}

#[test]
fn in_progress_marker_is_reported() {
    let repo = seeded();
    let (_watcher, rx) = repo.watch();
    repo.write(
        ".git/MERGE_HEAD",
        "0000000000000000000000000000000000000000\n",
    );
    let all = union(&collect(&rx));
    assert!(all.contains(&Area::InProgress), "{all:?}");
}

#[test]
fn burst_of_writes_gives_one_event() {
    let repo = seeded();
    let (_watcher, rx) = repo.watch();
    for i in 0..100 {
        repo.write(&format!("burst/{i}.txt"), "x\n");
    }
    let events = collect(&rx);
    assert_eq!(events, vec![BTreeSet::from([Area::Worktree])]);
}

#[test]
fn writes_under_objects_alone_give_no_event() {
    let repo = seeded();
    let (_watcher, rx) = repo.watch();
    repo.write(".git/objects/zz/deadbeef", "junk");
    repo.write(".git/objects/info/extra", "junk");
    silent(&rx);
}

#[test]
fn ignored_paths_give_no_event() {
    let repo = seeded();
    let (_watcher, rx) = repo.watch();
    repo.write("target/debug/out.o", "x");
    repo.write("build.log", "x");
    silent(&rx);
    // A non-ignored change right after still comes through.
    repo.write("c.txt", "c\n");
    assert_eq!(collect(&rx), vec![BTreeSet::from([Area::Worktree])]);
}

#[test]
fn editing_gitignore_is_a_worktree_change() {
    let repo = seeded();
    let (_watcher, rx) = repo.watch();
    repo.write(".gitignore", "target/\n");
    assert_eq!(collect(&rx), vec![BTreeSet::from([Area::Worktree])]);
}

#[test]
fn dropping_the_watcher_stops_events() {
    let repo = seeded();
    let (watcher, rx) = repo.watch();
    drop(watcher);
    repo.write("a.txt", "three\n");
    silent(&rx);
}

#[test]
fn start_fails_for_a_missing_folder() {
    let repo = seeded();
    let found = discover(&repo.dir).unwrap();
    fs::remove_dir_all(Path::new(&repo.dir)).unwrap();
    let (tx, _rx) = mpsc::channel();
    assert!(RepoWatcher::start(&found, tx).is_err());
}
