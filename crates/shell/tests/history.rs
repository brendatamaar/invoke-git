//! The history walk and commit metadata (task 18), on generated repos with
//! real git as the oracle. System and global git config are shut out.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use proptest::prelude::*;
use shell::history::{CommitId, HistoryEvent, read_meta, walk};
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
            "invoke-git-history-{}-{}",
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

    fn commit(&self, name: &str, message: &str) {
        fs::write(self.paths.workdir.join(name), message).unwrap();
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Runs the walk to the end and returns every commit in order.
fn collect(paths: &RepoPaths) -> Vec<graph::Commit<CommitId>> {
    let rx = walk(paths.clone(), None, Arc::new(AtomicBool::new(false)));
    let mut all = Vec::new();
    loop {
        match rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the walk ended")
        {
            HistoryEvent::Started { .. } => {}
            HistoryEvent::Page(page) => all.extend(page),
            HistoryEvent::Done { commits } => {
                assert_eq!(commits, all.len());
                return all;
            }
            HistoryEvent::Failed(error) => panic!("walk failed: {error}"),
        }
    }
}

fn hex(id: &CommitId) -> String {
    id.to_hex().to_string()
}

/// `git rev-list --parents --all` as `commit -> parents`.
fn oracle(repo: &Repo) -> HashMap<String, Vec<String>> {
    repo.git(&["rev-list", "--parents", "--all"])
        .lines()
        .map(|line| {
            let mut words = line.split(' ').map(str::to_owned);
            (words.next().unwrap(), words.collect())
        })
        .collect()
}

/// A history with a branch, a merge, a side branch only a tag points at,
/// and a branch that is not checked out.
fn tangled() -> Repo {
    let repo = Repo::new();
    repo.commit("a", "one");
    repo.commit("b", "two");
    repo.git(&["switch", "-q", "-c", "feature"]);
    repo.commit("c", "feature one");
    repo.commit("d", "feature two");
    repo.git(&["switch", "-q", "main"]);
    repo.commit("e", "three");
    repo.git(&["merge", "-q", "--no-ff", "-m", "merge feature", "feature"]);
    repo.git(&["switch", "-q", "-c", "lonely", "HEAD~2"]);
    repo.commit("f", "lonely one");
    repo.git(&["tag", "v1"]);
    repo.git(&["switch", "-q", "main"]);
    repo.git(&["branch", "-q", "-D", "lonely"]);
    repo
}

#[test]
fn every_commit_once_with_the_parents_git_reports() {
    let repo = tangled();
    let commits = collect(&repo.paths);
    let want = oracle(&repo);
    let got: HashMap<String, Vec<String>> = commits
        .iter()
        .map(|c| (hex(&c.id), c.parents.iter().map(hex).collect()))
        .collect();
    assert_eq!(commits.len(), want.len(), "each commit exactly once");
    assert_eq!(got, want);
}

#[test]
fn children_come_before_their_parents() {
    let repo = tangled();
    let commits = collect(&repo.paths);
    let position: HashMap<_, _> = commits.iter().enumerate().map(|(i, c)| (c.id, i)).collect();
    for (index, commit) in commits.iter().enumerate() {
        for parent in &commit.parents {
            assert!(position[parent] > index, "{} before its child", hex(parent));
        }
    }
}

#[test]
fn a_commit_only_a_tag_reaches_is_walked() {
    let repo = tangled();
    let lonely = repo.git(&["rev-parse", "v1"]);
    let commits = collect(&repo.paths);
    assert!(commits.iter().any(|c| hex(&c.id) == lonely));
}

#[test]
fn an_unborn_repo_has_no_commits() {
    let repo = Repo::new();
    assert!(collect(&repo.paths).is_empty());
}

#[test]
fn a_shallow_clone_flags_its_boundary() {
    let origin = Repo::new();
    for n in 0..4 {
        origin.commit(&format!("f{n}"), &format!("commit {n}"));
    }
    let clone_dir = origin.root.join("shallow");
    let url = format!("file://{}", origin.paths.workdir.display()).replace('\\', "/");
    origin.git_in(
        &origin.root,
        &[
            "clone",
            "-q",
            "--depth",
            "2",
            &url,
            clone_dir.to_str().unwrap(),
        ],
    );
    let paths = RepoPaths {
        git_dir: clone_dir.join(".git"),
        workdir: clone_dir.clone(),
    };
    let commits = collect(&paths);
    assert_eq!(commits.len(), 2);
    let flagged: Vec<_> = commits.iter().filter(|c| c.truncated).collect();
    assert_eq!(flagged.len(), 1, "exactly the boundary commit is truncated");
    assert_eq!(hex(&flagged[0].id), hex(&commits[1].id), "the oldest one");
}

#[test]
fn metadata_matches_git_log() {
    let repo = Repo::new();
    repo.commit("a", "first line\n\nbody text");
    repo.commit("b", "second");
    let commits = collect(&repo.paths);
    let ids: Vec<_> = commits.iter().map(|c| c.id).collect();
    let meta = read_meta(&repo.paths, &ids).unwrap();
    for (id, meta) in ids.iter().zip(meta) {
        let meta = meta.expect("readable");
        let subject = repo.git(&["log", "-1", "--format=%s", &hex(id)]);
        let seconds: i64 = repo
            .git(&["log", "-1", "--format=%at", &hex(id)])
            .parse()
            .unwrap();
        assert_eq!(meta.subject, subject);
        assert_eq!(meta.author, "Ada Lovelace");
        assert_eq!(meta.seconds, seconds);
    }
}

#[test]
fn metadata_of_a_missing_commit_is_none() {
    let repo = Repo::new();
    repo.commit("a", "x");
    let missing = CommitId::from_hex(b"0123456789012345678901234567890123456789").unwrap();
    let meta = read_meta(&repo.paths, &[missing]).unwrap();
    assert_eq!(meta, vec![None]);
}

#[test]
fn a_cancelled_walk_stops_without_a_done() {
    let repo = Repo::new();
    for n in 0..30 {
        repo.commit(&format!("f{n}"), &format!("c{n}"));
    }
    let cancel = Arc::new(AtomicBool::new(true));
    let rx = walk(repo.paths.clone(), None, cancel);
    let mut done = false;
    while let Ok(event) = rx.recv_timeout(Duration::from_secs(10)) {
        if matches!(event, HistoryEvent::Done { .. }) {
            done = true;
        }
    }
    assert!(!done, "a cancelled walk must not report Done");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, .. ProptestConfig::default() })]

    /// Random histories: branches, merges. Everything git lists is walked
    /// once, parents agree, and the order is topological.
    #[test]
    fn random_histories_walk_like_git(steps in proptest::collection::vec(0u8..4, 3..14)) {
        let repo = Repo::new();
        repo.commit("root", "root");
        let mut branches = vec!["main".to_owned()];
        for (n, step) in steps.iter().enumerate() {
            let current = branches[n % branches.len()].clone();
            repo.git(&["switch", "-q", &current]);
            match step {
                0 | 1 => repo.commit(&format!("f{n}"), &format!("c{n}")),
                2 => {
                    let name = format!("b{n}");
                    repo.git(&["switch", "-q", "-c", &name]);
                    repo.commit(&format!("g{n}"), &format!("branch {n}"));
                    branches.push(name);
                }
                _ => {
                    if branches.len() > 1 {
                        let other = branches[(n + 1) % branches.len()].clone();
                        if other != current {
                            // A conflict-free merge: every commit touches its own file.
                            repo.git(&["merge", "-q", "--no-edit", "-m", "merge", &other]);
                        }
                    }
                }
            }
        }
        let commits = collect(&repo.paths);
        let want = oracle(&repo);
        let got: HashSet<String> = commits.iter().map(|c| hex(&c.id)).collect();
        prop_assert_eq!(commits.len(), want.len());
        prop_assert_eq!(got, want.keys().cloned().collect::<HashSet<_>>());
        let position: HashMap<_, _> = commits.iter().enumerate().map(|(i, c)| (c.id, i)).collect();
        for (index, commit) in commits.iter().enumerate() {
            let parents: Vec<String> = commit.parents.iter().map(hex).collect();
            prop_assert_eq!(&parents, &want[&hex(&commit.id)]);
            for parent in &commit.parents {
                prop_assert!(position[parent] > index);
            }
        }
    }
}

/// Timing on a big real repository, for the task 18 notes. Not part of the
/// suite: `INVOKE_GIT_BIG_REPO=C:\path\to\repo cargo test -p shell --release
/// --test history -- --ignored --nocapture`.
#[test]
#[ignore]
fn timing_on_a_big_repository() {
    let Some(path) = std::env::var_os("INVOKE_GIT_BIG_REPO") else {
        eprintln!("set INVOKE_GIT_BIG_REPO to run this");
        return;
    };
    let workdir = PathBuf::from(path);
    let paths = RepoPaths {
        git_dir: workdir.join(".git"),
        workdir,
    };
    let started = std::time::Instant::now();
    let rx = walk(paths, None, Arc::new(AtomicBool::new(false)));
    let mut layout = graph::Layout::<CommitId>::new();
    let (mut first_page, mut commits, mut layout_time) = (None, 0usize, Duration::ZERO);
    loop {
        match rx
            .recv_timeout(Duration::from_secs(300))
            .expect("the walk ended")
        {
            HistoryEvent::Started { commit_graph } => {
                eprintln!("commit-graph file: {commit_graph}")
            }
            HistoryEvent::Page(page) => {
                first_page.get_or_insert_with(|| started.elapsed());
                let before = std::time::Instant::now();
                layout.push_page(&page);
                layout_time += before.elapsed();
                commits += page.len();
            }
            HistoryEvent::Done { .. } => break,
            HistoryEvent::Failed(error) => panic!("{error}"),
        }
    }
    eprintln!(
        "{commits} commits: first page after {:?}, walk and layout done after {:?}, layout alone {:?}",
        first_page.unwrap_or_default(),
        started.elapsed(),
        layout_time
    );
}
