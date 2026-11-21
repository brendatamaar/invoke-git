//! Repo discovery and state reads against real git in generated temp repos.
//! System and global git config are shut out, so the user's setup never
//! leaks in. git is used to build fixtures and as the oracle for refs and
//! conflicts; the code under test only reads through gix and plain files.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use invoke_core::{
    Command as CoreCommand, Conflicts, DiskState, ErrorClass, Event, Expect, GitArgs, Head,
    IndexId, LockState, OpId, Phase, ReadId, ReadScope, RefName, RepoPath,
};
use shell::precondition::{self, RepoPaths};
use shell::{DiscoverError, discover, execute_read, read_state};

/// A temp folder holding one or more repos, deleted on drop.
struct Sandbox {
    root: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl Sandbox {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let root = std::env::temp_dir().join(format!(
            "invoke-git-read-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
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
        Self { root, env }
    }

    /// Runs git in `dir`, panicking on failure.
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.run(dir, args);
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// Runs git in `dir` and expects it to fail (a conflict, a stopped rebase).
    fn git_stops(&self, dir: &Path, args: &[&str]) {
        let output = self.run(dir, args);
        assert!(!output.status.success(), "git {args:?} should have stopped");
    }

    fn run(&self, dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .env("GIT_EDITOR", "true")
            .output()
            .unwrap()
    }

    /// A fresh repo on branch `main` in `<root>/<name>`.
    fn repo(&self, name: &str) -> Repo<'_> {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        self.git(&dir, &["init", "-q", "-b", "main"]);
        Repo { sandbox: self, dir }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Repo<'a> {
    sandbox: &'a Sandbox,
    dir: PathBuf,
}

impl Repo<'_> {
    fn git(&self, args: &[&str]) -> String {
        self.sandbox.git(&self.dir, args)
    }

    fn git_stops(&self, args: &[&str]) {
        self.sandbox.git_stops(&self.dir, args);
    }

    fn write(&self, path: &str, text: &str) {
        fs::write(self.dir.join(path), text).unwrap();
    }

    /// Writes `path`, stages it and commits.
    fn commit_file(&self, path: &str, text: &str, message: &str) {
        self.write(path, text);
        self.git(&["add", path]);
        self.git(&["commit", "-q", "-m", message]);
    }

    fn paths(&self) -> RepoPaths {
        RepoPaths {
            workdir: self.dir.clone(),
            git_dir: self.dir.join(".git"),
        }
    }

    fn read(&self) -> DiskState {
        read_state(&self.paths()).unwrap()
    }

    /// `main` and `topic` both change `a.txt` after a shared first commit,
    /// plus `topic` adds files before and after, so a rebase of `topic`
    /// onto `main` stops at step 2 of 3. Leaves `main` checked out.
    fn diverge(&self) {
        self.commit_file("a.txt", "base\n", "base");
        self.git(&["checkout", "-q", "-b", "topic"]);
        self.commit_file("t1.txt", "one\n", "topic 1");
        self.commit_file("a.txt", "topic\n", "topic 2");
        self.commit_file("t3.txt", "three\n", "topic 3");
        self.git(&["checkout", "-q", "main"]);
        self.commit_file("a.txt", "main\n", "main change");
    }
}

/// Same folder, whatever the spelling (`\\?\` prefixes, short names, symlinks).
fn assert_same_dir(found: &Path, expected: &Path) {
    assert_eq!(
        fs::canonicalize(found).unwrap(),
        fs::canonicalize(expected).unwrap()
    );
}

fn conflict_paths(names: &[&str]) -> Conflicts {
    Conflicts {
        paths: names.iter().map(|name| RepoPath((*name).into())).collect(),
    }
}

/// `git for-each-ref` as the oracle for the refs snapshot.
fn oracle_refs(repo: &Repo) -> BTreeMap<RefName, String> {
    let listing = repo.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
    listing
        .lines()
        .map(|line| {
            let (name, id) = line.split_once(' ').unwrap();
            (RefName(name.into()), id.to_owned())
        })
        .collect()
}

fn refs_as_hex(disk: &DiskState) -> BTreeMap<RefName, String> {
    disk.refs
        .refs
        .iter()
        .map(|(name, oid)| (name.clone(), oid.as_hex().to_owned()))
        .collect()
}

/// `git ls-files -u` as the oracle for conflicts.
fn oracle_conflicts(repo: &Repo) -> BTreeSet<String> {
    repo.git(&["ls-files", "-u"])
        .lines()
        .map(|line| line.split('\t').nth(1).unwrap().to_owned())
        .collect()
}

fn conflicts_of(phase: &Phase) -> &Conflicts {
    match phase {
        Phase::Merging { conflicts }
        | Phase::CherryPicking { conflicts }
        | Phase::Reverting { conflicts }
        | Phase::Rebasing { conflicts, .. } => conflicts,
        Phase::Clean => panic!("clean has no conflicts"),
    }
}

#[test]
fn clean_repo_reads_head_refs_index_and_lock() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("clean");
    repo.commit_file("a.txt", "one\n", "first");
    repo.git(&["tag", "v1"]);
    repo.git(&["tag", "-a", "-m", "annotated", "v2"]);
    repo.git(&["branch", "other"]);
    let disk = repo.read();

    let head = repo.git(&["rev-parse", "HEAD"]);
    match &disk.head {
        Head::Branch { name, oid } => {
            assert_eq!(name.0, "main");
            assert_eq!(oid.as_hex(), head);
        }
        other => panic!("unexpected head {other:?}"),
    }
    assert_eq!(disk.phase, Phase::Clean);
    assert_eq!(disk.lock, LockState::Free);
    assert!(matches!(disk.index, IndexId::Checksum(_)));
    assert!(disk.status.entries.is_empty());
    // Loose branches and tags; the annotated tag names its tag object.
    assert_eq!(refs_as_hex(&disk), oracle_refs(&repo));
    assert_eq!(disk.refs.refs.len(), 4);
    // The snapshot and the write check agree on head and index.
    let expect = precondition::current(&repo.paths()).unwrap();
    assert_eq!(
        expect,
        Expect {
            head: disk.head.clone(),
            index: disk.index.clone()
        }
    );
}

#[test]
fn unborn_branch_has_no_refs_and_no_index() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("unborn");
    let disk = repo.read();
    assert_eq!(
        disk.head,
        Head::Unborn(invoke_core::BranchName("main".into()))
    );
    assert_eq!(disk.index, IndexId::Absent);
    assert!(disk.refs.refs.is_empty());
    assert_eq!(disk.phase, Phase::Clean);
}

#[test]
fn detached_head_is_read_as_detached() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("detached");
    repo.commit_file("a.txt", "one\n", "first");
    repo.git(&["checkout", "-q", "--detach"]);
    let disk = repo.read();
    match disk.head {
        Head::Detached(oid) => assert_eq!(oid.as_hex(), repo.git(&["rev-parse", "HEAD"])),
        other => panic!("unexpected head {other:?}"),
    }
}

#[test]
fn merge_with_conflicts_reports_the_conflicted_paths() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("merge");
    repo.diverge();
    repo.commit_file("b.txt", "main b\n", "main b");
    repo.git(&["checkout", "-q", "topic"]);
    repo.commit_file("b.txt", "topic b\n", "topic b");
    repo.git_stops(&["merge", "main"]);
    let disk = repo.read();
    assert!(
        matches!(disk.phase, Phase::Merging { .. }),
        "{:?}",
        disk.phase
    );
    assert_eq!(
        conflicts_of(&disk.phase),
        &conflict_paths(&["a.txt", "b.txt"])
    );
    assert_eq!(
        oracle_conflicts(&repo),
        BTreeSet::from(["a.txt".to_owned(), "b.txt".to_owned()])
    );

    // Resolved but not yet committed: still merging, no conflicts left.
    repo.write("a.txt", "resolved\n");
    repo.write("b.txt", "resolved\n");
    repo.git(&["add", "a.txt", "b.txt"]);
    let disk = repo.read();
    assert_eq!(
        disk.phase,
        Phase::Merging {
            conflicts: Conflicts::default()
        }
    );

    repo.git(&["commit", "-q", "--no-edit"]);
    assert_eq!(repo.read().phase, Phase::Clean);
}

#[test]
fn stopped_rebase_reports_step_and_total() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("rebase");
    repo.diverge();
    repo.git(&["checkout", "-q", "topic"]);
    repo.git_stops(&["rebase", "main"]);
    assert!(repo.dir.join(".git/rebase-merge").is_dir());
    let disk = repo.read();
    assert_eq!(
        disk.phase,
        Phase::Rebasing {
            step: 2,
            total: 3,
            conflicts: conflict_paths(&["a.txt"])
        }
    );
    // During a rebase HEAD is detached.
    assert!(matches!(disk.head, Head::Detached(_)));

    repo.git(&["rebase", "--abort"]);
    assert_eq!(repo.read().phase, Phase::Clean);
}

#[test]
fn stopped_apply_rebase_reads_next_and_last() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("rebase-apply");
    repo.diverge();
    repo.git(&["checkout", "-q", "topic"]);
    repo.git_stops(&["rebase", "--apply", "main"]);
    assert!(repo.dir.join(".git/rebase-apply").is_dir());
    assert_eq!(
        repo.read().phase,
        Phase::Rebasing {
            step: 2,
            total: 3,
            conflicts: conflict_paths(&["a.txt"])
        }
    );
}

#[test]
fn cherry_pick_with_conflict_is_cherry_picking() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("pick");
    repo.diverge();
    repo.git_stops(&["cherry-pick", "topic~1"]);
    let disk = repo.read();
    assert_eq!(
        disk.phase,
        Phase::CherryPicking {
            conflicts: conflict_paths(&["a.txt"])
        }
    );
}

#[test]
fn revert_with_conflict_is_reverting() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("revert");
    repo.commit_file("a.txt", "one\n", "one");
    repo.commit_file("a.txt", "two\n", "two");
    repo.commit_file("a.txt", "three\n", "three");
    // Reverting "two" now clashes with "three".
    repo.git_stops(&["revert", "--no-edit", "HEAD~1"]);
    let disk = repo.read();
    assert_eq!(
        disk.phase,
        Phase::Reverting {
            conflicts: conflict_paths(&["a.txt"])
        }
    );
}

#[test]
fn held_index_lock_is_reported() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("lock");
    repo.commit_file("a.txt", "one\n", "first");
    let lock = repo.dir.join(".git/index.lock");
    fs::write(&lock, "").unwrap();
    assert_eq!(repo.read().lock, LockState::HeldByOther);
    fs::remove_file(&lock).unwrap();
    assert_eq!(repo.read().lock, LockState::Free);
}

#[test]
fn packed_and_loose_refs_are_both_read() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("packed");
    repo.commit_file("a.txt", "one\n", "first");
    repo.git(&["branch", "packed-one"]);
    repo.git(&["tag", "packed-tag"]);
    repo.git(&["pack-refs", "--all", "--prune"]);
    assert!(!repo.dir.join(".git/refs/heads/packed-one").exists());
    repo.commit_file("a.txt", "two\n", "second");
    repo.git(&["branch", "loose-one"]);
    let disk = repo.read();
    assert_eq!(refs_as_hex(&disk), oracle_refs(&repo));
    for name in [
        "refs/heads/packed-one",
        "refs/tags/packed-tag",
        "refs/heads/loose-one",
    ] {
        assert!(disk.refs.refs.contains_key(&RefName(name.into())), "{name}");
    }
    // `main` is packed but was moved by the second commit: loose wins.
    let main = &disk.refs.refs[&RefName("refs/heads/main".into())];
    assert_eq!(main.as_hex(), repo.git(&["rev-parse", "HEAD"]));
}

#[test]
fn remote_head_symref_resolves_to_its_target() {
    let sandbox = Sandbox::new();
    let origin = sandbox.repo("origin");
    origin.commit_file("a.txt", "one\n", "first");
    let clone = sandbox.root.join("clone");
    sandbox.git(
        &sandbox.root,
        &[
            "clone",
            "-q",
            origin.dir.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let repo = Repo {
        sandbox: &sandbox,
        dir: clone,
    };
    let disk = repo.read();
    assert_eq!(refs_as_hex(&disk), oracle_refs(&repo));
    assert!(
        disk.refs
            .refs
            .contains_key(&RefName("refs/remotes/origin/HEAD".into()))
    );
}

#[test]
fn discovery_works_from_the_root_and_from_a_subdirectory() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("sub");
    fs::create_dir_all(repo.dir.join("deep/er")).unwrap();
    repo.commit_file("deep/er/a.txt", "one\n", "first");
    for start in [
        repo.dir.clone(),
        repo.dir.join("deep/er"),
        repo.dir.join(".git"),
    ] {
        let found = discover(&start).unwrap();
        assert_same_dir(&found.paths.workdir, &repo.dir);
        assert_same_dir(&found.paths.git_dir, &repo.dir.join(".git"));
        assert_same_dir(&found.common_dir, &found.paths.git_dir);
        assert_eq!(read_state(&found.paths).unwrap(), repo.read());
    }
}

#[test]
fn bare_repos_and_plain_folders_are_refused() {
    let sandbox = Sandbox::new();
    let bare = sandbox.root.join("bare.git");
    fs::create_dir_all(&bare).unwrap();
    sandbox.git(&bare, &["init", "-q", "--bare", "-b", "main"]);
    match discover(&bare) {
        Err(error @ DiscoverError::Bare { .. }) => {
            assert!(error.to_string().contains("bare repository"), "{error}");
        }
        other => panic!("bare repo not refused: {other:?}"),
    }

    let plain = sandbox.root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    match discover(&plain) {
        Err(error @ DiscoverError::NotARepo { .. }) => {
            assert!(error.to_string().contains("not inside a git repository"));
        }
        other => panic!("plain folder not refused: {other:?}"),
    }
}

#[test]
fn git_file_pointing_elsewhere_is_followed() {
    let sandbox = Sandbox::new();
    let work = sandbox.root.join("work");
    let separate = sandbox.root.join("separate-git-dir");
    fs::create_dir_all(&work).unwrap();
    sandbox.git(
        &work,
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            separate.to_str().unwrap(),
        ],
    );
    assert!(work.join(".git").is_file());
    fs::write(work.join("a.txt"), "one\n").unwrap();
    sandbox.git(&work, &["add", "a.txt"]);
    sandbox.git(&work, &["commit", "-q", "-m", "first"]);
    fs::create_dir_all(work.join("sub")).unwrap();

    let found = discover(&work.join("sub")).unwrap();
    assert_same_dir(&found.paths.workdir, &work);
    assert_same_dir(&found.paths.git_dir, &separate);
    let disk = read_state(&found.paths).unwrap();
    assert!(matches!(disk.head, Head::Branch { .. }));
    assert_eq!(disk.phase, Phase::Clean);

    // A marker in the separate git dir is found there, not under `.git`.
    fs::write(separate.join("MERGE_HEAD"), "0".repeat(40) + "\n").unwrap();
    assert!(matches!(
        read_state(&found.paths).unwrap().phase,
        Phase::Merging { .. }
    ));
}

#[test]
fn linked_worktree_has_its_own_markers_and_shares_refs() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("main-wt");
    repo.diverge();
    let linked = sandbox.root.join("linked");
    repo.git(&["worktree", "add", "-q", linked.to_str().unwrap(), "topic"]);
    assert!(linked.join(".git").is_file());

    // A conflicted merge in the linked worktree only.
    sandbox.git(&linked, &["checkout", "-q", "-b", "side"]);
    fs::write(linked.join("a.txt"), "side\n").unwrap();
    sandbox.git(&linked, &["commit", "-q", "-am", "side"]);
    sandbox.git_stops(&linked, &["merge", "main"]);

    fs::create_dir_all(linked.join("sub")).unwrap();
    let found = discover(&linked.join("sub")).unwrap();
    assert_same_dir(&found.paths.workdir, &linked);
    assert_same_dir(
        &found.paths.git_dir,
        &repo.dir.join(".git/worktrees/linked"),
    );
    assert_same_dir(&found.common_dir, &repo.dir.join(".git"));
    assert!(found.paths.git_dir.join("MERGE_HEAD").exists());

    let in_linked = read_state(&found.paths).unwrap();
    assert_eq!(
        in_linked.phase,
        Phase::Merging {
            conflicts: conflict_paths(&["a.txt"])
        }
    );
    match &in_linked.head {
        Head::Branch { name, .. } => assert_eq!(name.0, "side"),
        other => panic!("unexpected head {other:?}"),
    }
    // Refs come from the common dir: the linked worktree sees `main`.
    assert_eq!(refs_as_hex(&in_linked), oracle_refs(&repo));
    assert!(
        in_linked
            .refs
            .refs
            .contains_key(&RefName("refs/heads/side".into()))
    );

    // The main worktree is untouched by the linked worktree's merge.
    let in_main = repo.read();
    assert_eq!(in_main.phase, Phase::Clean);
    match &in_main.head {
        Head::Branch { name, .. } => assert_eq!(name.0, "main"),
        other => panic!("unexpected head {other:?}"),
    }
    // And the index ids differ: each worktree has its own index.
    assert_ne!(in_main.index, in_linked.index);
}

#[test]
fn index_id_tracks_the_index_even_with_skip_hash() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("skiphash");
    repo.git(&["config", "index.skipHash", "true"]);
    repo.commit_file("a.txt", "one\n", "first");
    repo.write("b.txt", "two\n");
    repo.git(&["add", "b.txt"]);
    let index = fs::read(repo.dir.join(".git/index")).unwrap();
    assert!(
        index[index.len() - 20..].iter().all(|byte| *byte == 0),
        "git did not skip the index hash"
    );
    let before = repo.read().index;
    assert!(matches!(&before, IndexId::Checksum(oid) if oid.as_hex().starts_with("0000")));
    repo.write("c.txt", "three\n");
    repo.git(&["add", "c.txt"]);
    let after = repo.read().index;
    assert_ne!(before, after);
    // The write check uses the same id.
    assert_eq!(precondition::current(&repo.paths()).unwrap().index, after);
}

#[test]
fn skip_hash_ids_depend_on_both_size_and_mtime() {
    let sandbox = Sandbox::new();
    let write = |name: &str, body_len: usize, trailer: u8, mtime_secs: u64| {
        let path = sandbox.root.join(name);
        let mut bytes = vec![b'x'; body_len];
        bytes.extend([trailer; 20]);
        fs::write(&path, bytes).unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(mtime_secs))
            .unwrap();
        precondition::index_id(&path, 20).unwrap()
    };
    let base = write("base", 8, 0, 1_000);
    assert_eq!(base, write("same", 8, 0, 1_000));
    assert_ne!(base, write("longer", 9, 0, 1_000), "size must count");
    assert_ne!(base, write("newer", 8, 0, 1_001), "mtime must count");
    // A real trailer is used as is, whatever the size and mtime.
    let real = write("real", 8, 0xab, 1_000);
    assert_eq!(real, write("real2", 12, 0xab, 5_000));
    assert!(matches!(&real, IndexId::Checksum(oid) if oid.as_hex() == "ab".repeat(20)));
}

#[test]
fn read_state_command_becomes_state_read_or_read_failed() {
    let sandbox = Sandbox::new();
    let repo = sandbox.repo("glue");
    repo.commit_file("a.txt", "one\n", "first");
    let read = CoreCommand::ReadState {
        read: ReadId(7),
        scope: ReadScope::full(),
    };
    match execute_read(&repo.paths(), &read) {
        Some(Event::StateRead { read, disk }) => {
            assert_eq!(read, ReadId(7));
            assert_eq!(disk, repo.read());
        }
        other => panic!("unexpected event {other:?}"),
    }

    let missing = RepoPaths {
        workdir: sandbox.root.join("nothing"),
        git_dir: sandbox.root.join("nothing/.git"),
    };
    match execute_read(&missing, &read) {
        Some(Event::ReadFailed { read, failure }) => {
            assert_eq!(read, ReadId(7));
            assert_eq!(failure.class, ErrorClass::Bug);
        }
        other => panic!("unexpected event {other:?}"),
    }

    // Writes are not reads.
    let cancel = CoreCommand::CancelOp(OpId(1));
    assert!(execute_read(&repo.paths(), &cancel).is_none());
    let write = CoreCommand::RunGit {
        op: OpId(2),
        args: GitArgs::Commit {
            message: "x".into(),
            amend: false,
        },
        expect: precondition::current(&repo.paths()).unwrap(),
    };
    assert!(execute_read(&repo.paths(), &write).is_none());
}
