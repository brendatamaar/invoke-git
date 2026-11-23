//! What a push would send (task 21), on generated repos with a local bare
//! remote and real git as the oracle. System and global git config are shut
//! out.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use invoke_core::RepoPath;
use proptest::prelude::*;
use shell::history::CommitId;
use shell::outgoing::{MAX_COMMITS, read_outgoing, read_range_sides};
use shell::precondition::RepoPaths;

struct Repo {
    root: PathBuf,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
    clock: std::cell::Cell<u32>,
}

impl Repo {
    /// A clone of a fresh bare remote that has one commit on `main`.
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "invoke-git-outgoing-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let global = root.join("gitconfig");
        fs::write(&global, "").unwrap();
        let env = [
            ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
            ("GIT_CONFIG_GLOBAL", global.into_os_string()),
            ("GIT_AUTHOR_NAME", "Ada Lovelace".into()),
            ("GIT_AUTHOR_EMAIL", "ada@example.com".into()),
            ("GIT_COMMITTER_NAME", "Ada Lovelace".into()),
            ("GIT_COMMITTER_EMAIL", "ada@example.com".into()),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v))
        .collect();
        let workdir = root.join("repo");
        let repo = Self {
            paths: RepoPaths {
                git_dir: workdir.join(".git"),
                workdir,
            },
            root,
            env,
            clock: std::cell::Cell::new(0),
        };
        let remote = repo.root.join("remote.git");
        repo.git_in(
            &repo.root,
            &["init", "-q", "--bare", "-b", "main", "remote.git"],
        );
        repo.git_in(
            &repo.root,
            &["clone", "-q", remote.to_str().unwrap(), "repo"],
        );
        repo.git(&["checkout", "-q", "-b", "main"]);
        repo.write("base.txt", "base\n");
        repo.commit("base");
        repo.git(&["push", "-q", "-u", "origin", "main"]);
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

    /// Commits with a strictly later time each time, so the order is by
    /// time and never a tie.
    fn commit(&self, message: &str) -> String {
        let tick = self.clock.get() + 1;
        self.clock.set(tick);
        let date = format!("2024-03-05T10:{:02}:{:02}+0000", tick / 60 % 60, tick % 60);
        self.git(&["add", "-A"]);
        let output = Command::new("git")
            .args(["commit", "-q", "--allow-empty", "-m", message])
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(output.status.success());
        self.git(&["rev-parse", "HEAD"])
    }

    /// Another clone pushes a commit to the remote, so the remote moves
    /// without this repo knowing; `fetch` brings it in.
    fn remote_moves(&self, file: &str) {
        let other = self.root.join("other");
        if !other.exists() {
            let remote = self.root.join("remote.git");
            self.git_in(
                &self.root,
                &["clone", "-q", remote.to_str().unwrap(), "other"],
            );
        }
        self.git_in(&other, &["pull", "-q"]);
        fs::write(other.join(file), "theirs\n").unwrap();
        self.git_in(&other, &["add", "-A"]);
        self.git_in(&other, &["commit", "-q", "-m", "remote change"]);
        self.git_in(&other, &["push", "-q"]);
        self.git(&["fetch", "-q"]);
    }

    fn up(&self) -> Option<&'static str> {
        Some("refs/remotes/origin/main")
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn ids(list: &[shell::outgoing::OutgoingCommit]) -> Vec<String> {
    list.iter().map(|c| c.id.clone()).collect()
}

#[test]
fn the_outgoing_commits_are_the_ones_the_upstream_lacks_newest_first() {
    let repo = Repo::new();
    repo.write("a.txt", "one\n");
    let first = repo.commit("add a");
    repo.write("a.txt", "one\ntwo\n");
    repo.write("dir/b.txt", "b\n");
    let second = repo.commit("grow a\n\nwith a body");
    let out = read_outgoing(&repo.paths, repo.up()).unwrap();
    assert_eq!(ids(&out.commits), vec![second.clone(), first.clone()]);
    assert_eq!(out.commits[0].subject, "grow a");
    assert_eq!(out.head, second);
    assert!(!out.truncated);
    let oracle = repo.git(&["rev-list", "origin/main..HEAD"]);
    assert_eq!(ids(&out.commits), oracle.lines().collect::<Vec<_>>());
    let names: Vec<&str> = out.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, vec!["a.txt", "dir/b.txt"]);
    assert_eq!(
        out.base.as_deref(),
        Some(repo.git(&["rev-parse", "origin/main"]).as_str())
    );
}

#[test]
fn nothing_is_outgoing_when_the_branch_is_level() {
    let repo = Repo::new();
    let out = read_outgoing(&repo.paths, repo.up()).unwrap();
    assert!(out.commits.is_empty() && out.files.is_empty());
    assert_eq!(out.base.as_deref(), Some(out.head.as_str()));
}

#[test]
fn the_combined_diff_leaves_out_what_only_the_remote_changed() {
    // `@{u}...HEAD`: from the merge base, so a change that arrived on the
    // remote is not shown as if we were deleting it.
    let repo = Repo::new();
    repo.write("mine.txt", "mine\n");
    repo.commit("mine");
    repo.remote_moves("theirs.txt");
    let out = read_outgoing(&repo.paths, repo.up()).unwrap();
    let names: Vec<&str> = out.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, vec!["mine.txt"]);
    assert_eq!(out.commits.len(), 1);
    let oracle = repo.git(&["diff", "--name-only", "origin/main...HEAD"]);
    assert_eq!(names, oracle.lines().collect::<Vec<_>>());
    assert_eq!(
        out.base.as_deref(),
        Some(repo.git(&["merge-base", "origin/main", "HEAD"]).as_str())
    );
}

#[test]
fn a_branch_without_an_upstream_sends_what_no_remote_has() {
    let repo = Repo::new();
    repo.git(&["checkout", "-q", "-b", "topic"]);
    repo.write("t1.txt", "1\n");
    let one = repo.commit("t1");
    repo.write("t2.txt", "2\n");
    let two = repo.commit("t2");
    let out = read_outgoing(&repo.paths, None).unwrap();
    assert_eq!(ids(&out.commits), vec![two, one]);
    let names: Vec<&str> = out.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, vec!["t1.txt", "t2.txt"]);
    assert_eq!(
        out.base.as_deref(),
        Some(repo.git(&["rev-parse", "origin/main"]).as_str())
    );
    // An upstream that no longer exists is the same case.
    let gone = read_outgoing(&repo.paths, Some("refs/remotes/origin/vanished")).unwrap();
    assert_eq!(gone, out);
}

#[test]
fn a_repository_with_no_remote_sends_everything_from_nothing() {
    let repo = Repo::new();
    repo.git(&["remote", "remove", "origin"]);
    let out = read_outgoing(&repo.paths, None).unwrap();
    assert_eq!(out.commits.len(), 1);
    assert_eq!(out.base, None);
    let names: Vec<&str> = out.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, vec!["base.txt"]);
}

#[test]
fn a_range_shows_both_sides_of_a_file() {
    let repo = Repo::new();
    repo.write("a.txt", "one\n");
    repo.commit("add a");
    repo.write("a.txt", "one\ntwo\n");
    repo.write("base.txt", "base\nchanged\n");
    repo.commit("grow a");
    let out = read_outgoing(&repo.paths, repo.up()).unwrap();
    let base = CommitId::from_hex(out.base.clone().unwrap().as_bytes()).unwrap();
    let head = CommitId::from_hex(out.head.as_bytes()).unwrap();
    let a = read_range_sides(&repo.paths, Some(&base), &head, &RepoPath("a.txt".into())).unwrap();
    assert_eq!(a.old, None);
    assert_eq!(a.new.as_deref(), Some("one\ntwo\n"));
    let b = read_range_sides(
        &repo.paths,
        Some(&base),
        &head,
        &RepoPath("base.txt".into()),
    )
    .unwrap();
    assert_eq!(b.old.as_deref(), Some("base\n"));
    assert_eq!(b.new.as_deref(), Some("base\nchanged\n"));
    // Against the empty tree everything is new.
    let all = read_range_sides(&repo.paths, None, &head, &RepoPath("base.txt".into())).unwrap();
    assert_eq!(all.old, None);
}

#[test]
fn a_long_list_stops_and_says_so() {
    let repo = Repo::new();
    // Without creating a thousand commits one by one: fast-import.
    let mut stream = String::new();
    let tip = repo.git(&["rev-parse", "HEAD"]);
    for n in 0..(MAX_COMMITS + 5) {
        let from = if n == 0 {
            format!("from {tip}\n")
        } else {
            String::new()
        };
        stream.push_str(&format!(
            "commit refs/heads/many\nmark :{m}\ncommitter T <t@t> {t} +0000\ndata 1\nx\n{from}M 100644 inline f.txt\ndata {len}\n{n}\n\n",
            m = n + 1,
            t = 1_700_000_000 + n,
            len = n.to_string().len() + 1,
        ));
    }
    let mut child = Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(&repo.paths.workdir)
        .envs(repo.env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    repo.git(&["checkout", "-q", "many"]);
    let out = read_outgoing(&repo.paths, repo.up()).unwrap();
    assert_eq!(out.commits.len(), MAX_COMMITS);
    assert!(out.truncated);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(12))]

    /// However many commits are made, and whether or not the remote moved
    /// meanwhile, the outgoing list and the combined file list are what git
    /// reports for `@{u}..HEAD` and `@{u}...HEAD`.
    #[test]
    fn it_agrees_with_git(count in 0usize..6, remote_moves: bool, touch_same in 0usize..3) {
        let repo = Repo::new();
        for n in 0..count {
            let file = format!("f{}.txt", n % (touch_same + 1));
            repo.write(&file, &format!("v{n}\n"));
            repo.commit(&format!("c{n}"));
        }
        if remote_moves {
            repo.remote_moves("remote-only.txt");
        }
        let out = read_outgoing(&repo.paths, repo.up()).unwrap();
        let listed = repo.git(&["rev-list", "origin/main..HEAD"]);
        prop_assert_eq!(ids(&out.commits), listed.lines().map(str::to_owned).collect::<Vec<_>>());
        let files = repo.git(&["diff", "--name-only", "origin/main...HEAD"]);
        let mut expected: Vec<String> = files.lines().map(str::to_owned).collect();
        expected.sort();
        let got: Vec<String> = out.files.iter().map(|f| f.path.clone()).collect();
        prop_assert_eq!(got, expected);
    }
}

#[test]
fn merged_branches_are_listed_by_time_not_by_parent_order() {
    let repo = Repo::new();
    repo.git(&["checkout", "-q", "-b", "topic"]);
    repo.write("t1.txt", "1\n");
    repo.commit("topic one");
    repo.write("t2.txt", "2\n");
    let topic_two = repo.commit("topic two");
    repo.git(&["checkout", "-q", "main"]);
    repo.write("m1.txt", "1\n");
    let main_one = repo.commit("main one");
    repo.write("m2.txt", "2\n");
    let main_two = repo.commit("main two");
    let tick = repo.clock.get() + 1;
    repo.clock.set(tick);
    let date = format!("2024-03-05T10:{:02}:{:02}+0000", tick / 60 % 60, tick % 60);
    let status = Command::new("git")
        .args(["merge", "-q", "--no-ff", "-m", "merge", "topic"])
        .current_dir(&repo.paths.workdir)
        .envs(repo.env.iter().map(|(k, v)| (k, v)))
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_AUTHOR_DATE", &date)
        .status()
        .unwrap();
    assert!(status.success());
    let merge = repo.git(&["rev-parse", "HEAD"]);
    let out = read_outgoing(&repo.paths, repo.up()).unwrap();
    let got = ids(&out.commits);
    assert_eq!(got.len(), 5);
    assert_eq!(&got[..3], &[merge, main_two, main_one]);
    assert_eq!(got[3], topic_two);
    assert!(out.commits[0].is_merge);
}
