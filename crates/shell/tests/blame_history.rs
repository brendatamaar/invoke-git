//! Blame and file history (P5) on generated repos. The oracle is git
//! itself, read a different way: `git blame --line-porcelain` (every line
//! carries its own metadata) and `git log --follow --format=%H`. System and
//! global git config are shut out.

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use shell::blame::{BlameEvent, BlameReader, FileHistoryEvent, ReadError};
use shell::git::Git;
use shell::precondition::RepoPaths;

struct Repo {
    root: PathBuf,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
    tick: AtomicU32,
}

impl Repo {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "invoke-git-blame-{}-{}",
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
            tick: AtomicU32::new(0),
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
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
        String::from_utf8(output.stdout).unwrap()
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        let path = self.paths.workdir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    /// Commits everything as `who`, one hour after the previous commit.
    fn commit(&self, who: &str, message: &str) {
        let n = self.tick.fetch_add(1, Ordering::SeqCst);
        let date = format!("2024-03-05T{:02}:00:00+0700", 1 + n);
        self.git(&["add", "-A"]);
        let email = format!("{who}@example.com");
        let output = Command::new("git")
            .args(["commit", "-q", "-m", message])
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .env("GIT_AUTHOR_NAME", who)
            .env("GIT_AUTHOR_EMAIL", &email)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_NAME", who)
            .env("GIT_COMMITTER_EMAIL", &email)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    fn reader(&self) -> BlameReader {
        BlameReader::new(Git::detect("git").unwrap(), self.paths.clone()).with_env(self.env.clone())
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Oracle row: (commit, author, email, author-time, final line, text).
type Row = (String, String, String, i64, u32, String);

fn oracle_blame(repo: &Repo, path: &str) -> Vec<Row> {
    let text = repo.git(&["blame", "--line-porcelain", "--", path]);
    let mut rows = Vec::new();
    let (mut id, mut line) = (String::new(), 0u32);
    let (mut author, mut email, mut time) = (String::new(), String::new(), 0i64);
    for l in text.split('\n') {
        if let Some(content) = l.strip_prefix('\t') {
            rows.push((
                id.clone(),
                author.clone(),
                email.clone(),
                time,
                line,
                content.trim_end_matches('\r').to_owned(),
            ));
        } else if let Some(v) = l.strip_prefix("author ") {
            author = v.to_owned();
        } else if let Some(v) = l.strip_prefix("author-mail ") {
            email = v.trim_matches(['<', '>']).to_owned();
        } else if let Some(v) = l.strip_prefix("author-time ") {
            time = v.parse().unwrap();
        } else {
            let mut parts = l.split(' ');
            let first = parts.next().unwrap_or("");
            if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
                id = first.to_owned();
                parts.next();
                line = parts.next().unwrap().parse().unwrap();
            }
        }
    }
    rows
}

fn read_blame(repo: &Repo, path: &str) -> Vec<shell::BlameLine> {
    repo.reader()
        .blame_blocking(None, path, &AtomicBool::new(false))
        .unwrap()
}

fn assert_matches_oracle(repo: &Repo, path: &str) {
    let got: Vec<Row> = read_blame(repo, path)
        .into_iter()
        .map(|l| (l.commit, l.author, l.email, l.seconds, l.line, l.text))
        .collect();
    let want = oracle_blame(repo, path);
    assert!(!want.is_empty());
    assert_eq!(got, want);
}

fn oracle_log(repo: &Repo, path: &str) -> Vec<String> {
    repo.git(&["log", "--follow", "--format=%H", "--", path])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn history(repo: &Repo, path: &str) -> Vec<shell::FileCommit> {
    repo.reader()
        .file_history_blocking(None, path, None, &AtomicBool::new(false))
        .unwrap()
}

#[test]
fn blame_of_a_new_file_matches_git() {
    let repo = Repo::new();
    repo.write("a.txt", b"one\ntwo\nthree\n");
    repo.commit("ada", "add a");
    assert_matches_oracle(&repo, "a.txt");
    let lines = read_blame(&repo, "a.txt");
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].summary, "add a");
    assert_eq!(lines[0].offset, 7 * 3600);
    assert_eq!(lines[0].path, "a.txt");
    assert!(lines.iter().all(|l| l.is_committed()));
}

#[test]
fn blame_attributes_edited_lines_to_their_authors() {
    let repo = Repo::new();
    repo.write("a.txt", b"one\ntwo\nthree\n");
    repo.commit("ada", "add a");
    repo.write("a.txt", b"one\nTWO\nthree\nfour\n");
    repo.commit("bob", "edit a");
    assert_matches_oracle(&repo, "a.txt");
    let lines = read_blame(&repo, "a.txt");
    let who: Vec<&str> = lines.iter().map(|l| l.author.as_str()).collect();
    assert_eq!(who, ["ada", "bob", "ada", "bob"]);
    assert_eq!(history(&repo, "a.txt").len(), 2);
}

#[test]
fn blame_follows_a_rename() {
    let repo = Repo::new();
    repo.write("old.txt", b"alpha\nbeta\ngamma\ndelta\nepsilon\n");
    repo.commit("ada", "add old");
    fs::create_dir_all(repo.paths.workdir.join("dir")).unwrap();
    repo.git(&["mv", "old.txt", "dir/new.txt"]);
    repo.commit("bob", "move");
    repo.write("dir/new.txt", b"alpha\nbeta\nGAMMA\ndelta\nepsilon\n");
    repo.commit("cy", "edit");
    assert_matches_oracle(&repo, "dir/new.txt");
    let lines = read_blame(&repo, "dir/new.txt");
    assert_eq!(lines[0].author, "ada");
    assert_eq!(lines[0].path, "old.txt");
    assert_eq!(lines[2].author, "cy");
    let log = history(&repo, "dir/new.txt");
    let ids: Vec<&str> = log.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, oracle_log(&repo, "dir/new.txt"));
    assert_eq!(ids.len(), 3);
    assert_eq!(log[0].path, "dir/new.txt");
    assert_eq!(log[2].path, "old.txt");
    // Without --follow the history would stop at the move.
    assert!(oracle_log(&repo, "dir/new.txt").len() > 2);
}

#[test]
fn crlf_lines_are_flagged_and_stripped() {
    let repo = Repo::new();
    repo.write("w.txt", b"dos\r\nunix\nmore dos\r\n");
    repo.commit("ada", "mixed endings");
    assert_matches_oracle(&repo, "w.txt");
    let lines = read_blame(&repo, "w.txt");
    let flags: Vec<bool> = lines.iter().map(|l| l.crlf).collect();
    assert_eq!(flags, [true, false, true]);
    assert_eq!(lines[0].text, "dos");
}

#[test]
fn large_file_matches_git() {
    let repo = Repo::new();
    let mut text = String::new();
    for i in 0..30_000 {
        text.push_str(&format!("line {i} {}\n", "x".repeat(i % 40)));
    }
    repo.write("big.txt", text.as_bytes());
    repo.commit("ada", "big");
    let edited: String = text
        .lines()
        .enumerate()
        .map(|(i, l)| {
            if i % 1000 == 0 {
                format!("edited {i}\n")
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    repo.write("big.txt", edited.as_bytes());
    repo.commit("bob", "touch some");
    assert_matches_oracle(&repo, "big.txt");
    assert_eq!(read_blame(&repo, "big.txt").len(), 30_000);
}

#[test]
fn uncommitted_lines_are_marked_and_a_revision_pins_the_blame() {
    let repo = Repo::new();
    repo.write("a.txt", b"one\ntwo\n");
    repo.commit("ada", "add a");
    repo.write("a.txt", b"one\ntwo\nthree\n");
    let lines = read_blame(&repo, "a.txt");
    assert!(lines[0].is_committed() && !lines[2].is_committed());
    let pinned = repo
        .reader()
        .blame_blocking(Some("HEAD"), "a.txt", &AtomicBool::new(false))
        .unwrap();
    assert_eq!(pinned.len(), 2);
}

#[test]
fn bad_input_is_refused_before_git_runs() {
    let repo = Repo::new();
    let cancel = AtomicBool::new(false);
    let reader = repo.reader();
    assert!(matches!(
        reader.blame_blocking(Some("--output=x"), "a.txt", &cancel),
        Err(ReadError::Failed(_))
    ));
    assert!(matches!(
        reader.file_history_blocking(None, "", None, &cancel),
        Err(ReadError::Failed(_))
    ));
    // A missing file is git's error, reported, not a panic.
    repo.write("a.txt", b"x\n");
    repo.commit("ada", "a");
    assert!(matches!(
        reader.blame_blocking(None, "nope.txt", &cancel),
        Err(ReadError::Failed(_))
    ));
}

#[test]
fn history_limit_and_channel_api() {
    let repo = Repo::new();
    for i in 0..4 {
        repo.write("a.txt", format!("v{i}\n").as_bytes());
        repo.commit("ada", &format!("c{i}"));
    }
    let rx = repo.reader().file_history(
        None,
        "a.txt".into(),
        Some(2),
        Arc::new(AtomicBool::new(false)),
    );
    match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
        FileHistoryEvent::Done(commits) => {
            let want: Vec<String> = oracle_log(&repo, "a.txt").into_iter().take(2).collect();
            let got: Vec<String> = commits.into_iter().map(|c| c.id).collect();
            assert_eq!(got, want);
        }
        FileHistoryEvent::Failed(e) => panic!("{e}"),
    }
    let rx = repo
        .reader()
        .blame(None, "a.txt".into(), Arc::new(AtomicBool::new(false)));
    match rx.recv_timeout(Duration::from_secs(10)).unwrap() {
        BlameEvent::Done(lines) => assert_eq!(lines.len(), 1),
        BlameEvent::Failed(e) => panic!("{e}"),
    }
}

#[test]
fn cancel_stops_the_read_and_sends_nothing() {
    let repo = Repo::new();
    repo.write("a.txt", b"one\n");
    repo.commit("ada", "a");
    let reader = repo.reader();
    // Already cancelled: no event, the channel just closes, quickly.
    let started = Instant::now();
    let rx = reader.blame(None, "a.txt".into(), Arc::new(AtomicBool::new(true)));
    assert!(rx.recv_timeout(Duration::from_secs(10)).is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    let rx = reader.file_history(None, "a.txt".into(), None, Arc::new(AtomicBool::new(true)));
    assert!(rx.recv_timeout(Duration::from_secs(10)).is_err());
    assert_eq!(
        reader.blame_blocking(None, "a.txt", &AtomicBool::new(true)),
        Err(ReadError::Cancelled)
    );
}

#[cfg(unix)]
#[test]
fn cancel_kills_a_slow_git() {
    use std::os::unix::fs::PermissionsExt;
    // A fake git that sleeps: proves the child is killed on cancel and the
    // caller is released well before the child would have finished.
    let dir = std::env::temp_dir().join(format!("invoke-git-slow-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let script = dir.join("git");
    fs::write(
        &script,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'git version 2.45.0'; exit 0; fi\nsleep 30\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let git = Git::detect(&script).unwrap();
    let paths = RepoPaths {
        workdir: dir.clone(),
        git_dir: dir.join(".git"),
    };
    let reader = BlameReader::new(git, paths);
    let cancel = Arc::new(AtomicBool::new(false));
    let rx = reader.blame(None, "a.txt".into(), cancel.clone());
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    cancel.store(true, Ordering::Relaxed);
    assert!(rx.recv_timeout(Duration::from_secs(10)).is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    let _ = fs::remove_dir_all(&dir);
}
