//! P10: integration tests on generated temp fixture repos for git worktrees,
//! submodules, and Git LFS pointer detection & operations.
//!
//! System and global git config are shut out so host configuration never leaks
//! in. Tests must pass both when `git-lfs` is installed and when it is NOT
//! installed.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use diff::{DiffOptions, Direction, Mode, PatchSpec, Pick, build_patch, diff_text};
use invoke_core::{
    BranchName, BranchStart, Command as CoreCommand, ErrorClass, Event, Head, LfsFileState, Notice,
    OpId, OpOutcome, Patch, Phase, RepoId, RepoPath, RepoState, SubmoduleOp, SubmoduleState,
    WorktreeSpec, open, same_worktree_path, step,
};
use shell::advanced::{git_lfs_version, inspect_lfs_file};
use shell::precondition::RepoPaths;
use shell::read::read_event;
use shell::{
    Git, GitRunner, InputsError, Source, StagingRunner, discover, load_inputs, read_state,
};

/// An isolated temporary directory for fixture repos, deleted on drop.
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
            "invoke-git-adv-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&root).unwrap();
        let global = root.join("gitconfig");
        // Allow local file:// protocol for submodule fixture repos inside the
        // temp sandbox only; production runs never alter user git config.
        fs::write(&global, "[protocol \"file\"]\n\tallow = always\n").unwrap();
        let env = [
            ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
            ("GIT_CONFIG_GLOBAL", global.into_os_string()),
            ("GIT_AUTHOR_NAME", "Test".into()),
            ("GIT_AUTHOR_EMAIL", "test@example.com".into()),
            ("GIT_COMMITTER_NAME", "Test".into()),
            ("GIT_COMMITTER_EMAIL", "test@example.com".into()),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v))
        .collect();
        Self { root, env }
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .env("GIT_EDITOR", "true")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} in {} failed:\nstdout: {}\nstderr: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn init_repo(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        fs::create_dir_all(&dir).unwrap();
        self.git(&dir, &["init", "-q", "-b", "main"]);
        dir
    }

    fn commit(&self, dir: &Path, path: &str, content: &[u8], message: &str) {
        let full = dir.join(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, content).unwrap();
        self.git(dir, &["add", path]);
        self.git(dir, &["commit", "-q", "-m", message]);
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn assert_same_dir(a: &Path, b: &Path) {
    assert_eq!(
        fs::canonicalize(a).unwrap(),
        fs::canonicalize(b).unwrap(),
        "{} != {}",
        a.display(),
        b.display()
    );
}

/// Sample valid Git LFS v1 pointer text.
const SAMPLE_LFS_POINTER: &str = "\
version https://git-lfs.github.com/spec/v1\n\
oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
size 12345\n";

const SAMPLE_LFS_POINTER_V2: &str = "\
version https://git-lfs.github.com/spec/v1\n\
oid sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
size 67890\n";

#[test]
fn linked_worktree_with_relative_git_file_pointer_is_discovered_independently() {
    let sb = Sandbox::new();
    let main = sb.init_repo("main");
    sb.commit(&main, "a.txt", b"main\n", "initial");
    sb.git(&main, &["branch", "feature"]);

    let wt_nested = main.join("wt-nested");
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            wt_nested.to_str().unwrap(),
            "feature",
        ],
    );
    // Rewrite `.git` to a relative pointer `../.git/worktrees/wt-nested` and
    // back-pointer `../../../wt-nested/.git` to cover relative-path worktrees.
    fs::write(
        wt_nested.join(".git"),
        "gitdir: ../.git/worktrees/wt-nested\n",
    )
    .unwrap();
    fs::write(
        main.join(".git/worktrees/wt-nested/gitdir"),
        "../../../wt-nested/.git\n",
    )
    .unwrap();
    fs::create_dir_all(wt_nested.join("sub/dir")).unwrap();

    let found_main = discover(&main).unwrap();
    let found_wt = discover(&wt_nested.join("sub/dir")).unwrap();

    assert_same_dir(&found_main.paths.workdir, &main);
    assert_same_dir(&found_wt.paths.workdir, &wt_nested);
    assert_same_dir(
        &found_wt.paths.git_dir,
        &main.join(".git/worktrees/wt-nested"),
    );
    assert_same_dir(&found_wt.common_dir, &found_main.common_dir);

    let main_state = read_state(&found_main.paths).unwrap();
    let wt_state = read_state(&found_wt.paths).unwrap();
    assert!(matches!(main_state.head, Head::Branch { ref name, .. } if name.0 == "main"));
    assert!(matches!(wt_state.head, Head::Branch { ref name, .. } if name.0 == "feature"));
    assert_eq!(main_state.phase, Phase::Clean);
    assert_eq!(wt_state.phase, Phase::Clean);

    // Both tabs see both worktrees, with `is_current` reflecting each tab.
    assert_eq!(main_state.advanced.worktrees.len(), 2);
    assert_eq!(wt_state.advanced.worktrees.len(), 2);
    assert!(main_state.advanced.worktrees[0].is_main);
    assert!(main_state.advanced.worktrees[0].is_current);
    assert!(!main_state.advanced.worktrees[1].is_main);
    assert!(!main_state.advanced.worktrees[1].is_current);

    assert!(wt_state.advanced.worktrees[0].is_main);
    assert!(!wt_state.advanced.worktrees[0].is_current);
    assert!(!wt_state.advanced.worktrees[1].is_main);
    assert!(wt_state.advanced.worktrees[1].is_current);
    assert!(same_worktree_path(
        &wt_state.advanced.worktrees[1].path,
        wt_nested.to_str().unwrap()
    ));
}

#[test]
fn worktree_list_detects_locked_and_prunable_worktrees() {
    let sb = Sandbox::new();
    let main = sb.init_repo("main-wt");
    sb.commit(&main, "a.txt", b"one\n", "c1");
    let locked_wt = sb.root.join("wt-locked");
    let gone_wt = sb.root.join("wt-gone");
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "b-locked",
            locked_wt.to_str().unwrap(),
        ],
    );
    sb.git(
        &main,
        &[
            "worktree",
            "lock",
            "--reason",
            "on usb drive",
            locked_wt.to_str().unwrap(),
        ],
    );
    sb.git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "b-gone",
            gone_wt.to_str().unwrap(),
        ],
    );
    fs::remove_dir_all(&gone_wt).unwrap();

    let found = discover(&main).unwrap();
    let state = read_state(&found.paths).unwrap();
    let wts = &state.advanced.worktrees;
    assert_eq!(wts.len(), 3);

    let locked = wts
        .iter()
        .find(|w| same_worktree_path(&w.path, locked_wt.to_str().unwrap()))
        .expect("locked worktree listed");
    assert_eq!(locked.locked.as_deref(), Some("on usb drive"));
    assert!(!locked.prunable);

    let gone = wts
        .iter()
        .find(|w| same_worktree_path(&w.path, gone_wt.to_str().unwrap()))
        .expect("prunable worktree listed");
    assert!(gone.prunable);
    assert_eq!(gone.locked, None);
}

#[test]
fn submodule_status_detects_clean_uninitialized_out_of_sync_and_dirty() {
    let sb = Sandbox::new();
    let child = sb.init_repo("child-lib");
    sb.commit(&child, "lib.txt", b"v1\n", "child v1");
    sb.commit(&child, "lib.txt", b"v2\n", "child v2");

    let super_repo = sb.init_repo("super");
    sb.commit(&super_repo, "README.md", b"hello\n", "super initial");
    sb.git(
        &super_repo,
        &[
            "submodule",
            "add",
            "-q",
            child.to_str().unwrap(),
            "libs/child",
        ],
    );
    sb.git(&super_repo, &["commit", "-q", "-m", "add submodule"]);

    // 1. Clean state in original superproject.
    let found_super = discover(&super_repo).unwrap();
    let state_clean = read_state(&found_super.paths).unwrap();
    assert_eq!(state_clean.advanced.submodules.len(), 1);
    let sub = &state_clean.advanced.submodules[0];
    assert_eq!(sub.path, RepoPath("libs/child".into()));
    assert_eq!(sub.state, SubmoduleState::Clean);
    assert!(sub.index_oid.is_some());
    assert_eq!(sub.index_oid, sub.head_oid);

    // Opening the submodule itself follows its `.git` file pointer cleanly.
    let sub_dir = super_repo.join("libs/child");
    assert!(sub_dir.join(".git").is_file());
    let found_sub = discover(&sub_dir).unwrap();
    assert_same_dir(&found_sub.paths.workdir, &sub_dir);
    assert_eq!(read_state(&found_sub.paths).unwrap().phase, Phase::Clean);

    // 2. Dirty state: edit a tracked file inside the submodule.
    fs::write(sub_dir.join("lib.txt"), b"dirty edit\n").unwrap();
    let state_dirty = read_state(&found_super.paths).unwrap();
    assert_eq!(
        state_dirty.advanced.submodules[0].state,
        SubmoduleState::Dirty
    );
    // Restore clean file.
    fs::write(sub_dir.join("lib.txt"), b"v2\n").unwrap();

    // 3. OutOfSync state: move submodule HEAD to HEAD~1 without updating superproject index.
    sb.git(&sub_dir, &["checkout", "-q", "HEAD~1"]);
    let state_oos = read_state(&found_super.paths).unwrap();
    assert_eq!(
        state_oos.advanced.submodules[0].state,
        SubmoduleState::OutOfSync
    );
    assert_ne!(
        state_oos.advanced.submodules[0].index_oid,
        state_oos.advanced.submodules[0].head_oid
    );

    // 4. Uninitialized state: fresh clone without `--recurse-submodules`.
    let cloned = sb.root.join("super-clone");
    sb.git(
        &sb.root,
        &[
            "clone",
            "-q",
            super_repo.to_str().unwrap(),
            cloned.to_str().unwrap(),
        ],
    );
    let found_clone = discover(&cloned).unwrap();
    let state_uninit = read_state(&found_clone.paths).unwrap();
    assert_eq!(state_uninit.advanced.submodules.len(), 1);
    assert_eq!(
        state_uninit.advanced.submodules[0].state,
        SubmoduleState::Uninitialized
    );
    assert!(state_uninit.advanced.submodules[0].index_oid.is_some());
    assert_eq!(state_uninit.advanced.submodules[0].head_oid, None);
}

#[test]
fn lfs_detection_works_for_pointer_downloaded_and_missing_without_git_lfs() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("lfs-repo");
    sb.commit(
        &repo,
        ".gitattributes",
        b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        "track bin with lfs",
    );
    // Simulate a broken/missing git-lfs filter in repo config to prove reads
    // and diff/staging never break when git-lfs is absent.
    sb.git(
        &repo,
        &[
            "config",
            "filter.lfs.clean",
            "nonexistent-git-lfs clean -- %f",
        ],
    );
    sb.git(&repo, &["config", "filter.lfs.required", "true"]);

    // Commit pointer directly (bypassing filter via update-index or clean repo first).
    sb.git(&repo, &["config", "--unset", "filter.lfs.required"]);
    sb.git(&repo, &["config", "--unset", "filter.lfs.clean"]);
    sb.commit(
        &repo,
        "asset.bin",
        SAMPLE_LFS_POINTER.as_bytes(),
        "add lfs pointer",
    );
    sb.git(
        &repo,
        &[
            "config",
            "filter.lfs.clean",
            "nonexistent-git-lfs clean -- %f",
        ],
    );
    sb.git(&repo, &["config", "filter.lfs.required", "true"]);

    let paths = RepoPaths {
        workdir: repo.clone(),
        git_dir: repo.join(".git"),
    };

    // 1. Pointer state detected in read_state and inspect_lfs_file.
    let state = read_state(&paths).unwrap();
    assert!(state.advanced.lfs.detected);
    assert_eq!(state.advanced.lfs.files.len(), 1);
    let file = &state.advanced.lfs.files[0];
    assert_eq!(file.path, RepoPath("asset.bin".into()));
    assert_eq!(file.state, LfsFileState::Pointer);
    assert_eq!(file.pointer.as_ref().unwrap().size, 12345);
    assert_eq!(
        inspect_lfs_file(&paths, &RepoPath("asset.bin".into())).unwrap(),
        *file
    );

    // 2. Modifying pointer text still loads diff & stages cleanly without git-lfs.
    fs::write(repo.join("asset.bin"), SAMPLE_LFS_POINTER_V2).unwrap();
    let path = RepoPath("asset.bin".into());
    let inputs = load_inputs(&paths, &path, Source::IndexToWorktree).unwrap();
    assert_eq!(inputs.old.as_deref(), Some(SAMPLE_LFS_POINTER));
    assert_eq!(inputs.new.as_deref(), Some(SAMPLE_LFS_POINTER_V2));

    let text_diff = diff_text(
        inputs.old.clone().unwrap(),
        inputs.new.clone().unwrap(),
        &DiffOptions::default(),
    );
    assert!(!text_diff.is_empty());

    let patch = build_patch(
        &text_diff,
        &PatchSpec {
            path: &path.0,
            direction: Direction::Stage,
            old_exists: true,
            new_exists: true,
            mode: if inputs.executable {
                Mode::Executable
            } else {
                Mode::Regular
            },
        },
        &Pick::File,
    )
    .unwrap()
    .unwrap();

    let git = Git::detect("git").unwrap();
    let staging = StagingRunner::new(git, paths.clone()).with_env(sb.env.clone());
    let event = staging.apply(OpId(1), &Patch(patch), &inputs.based_on);
    assert!(
        matches!(
            event,
            invoke_core::Event::OpFinished(OpId(1), OpOutcome::Succeeded)
        ),
        "staging an LFS pointer file must succeed: {event:?}"
    );

    // 3. Downloaded binary content (> 1024 bytes with NUL bytes) is detected as
    // `Downloaded`, and `load_inputs` reports `NotText` cleanly instead of failing.
    let mut binary = vec![0u8; 2048];
    binary[0] = 0x89;
    binary[1] = b'P';
    binary[2] = b'N';
    binary[3] = b'G';
    fs::write(repo.join("asset.bin"), &binary).unwrap();
    let state_dl = read_state(&paths).unwrap();
    let dl_file = state_dl.advanced.lfs.file(&path).unwrap();
    assert_eq!(dl_file.state, LfsFileState::Downloaded);
    assert_eq!(dl_file.pointer.as_ref().unwrap().size, 67890);
    assert!(matches!(
        load_inputs(&paths, &path, Source::IndexToWorktree),
        Err(InputsError::NotText(_))
    ));

    // 4. Missing worktree file is detected as `Missing`.
    fs::remove_file(repo.join("asset.bin")).unwrap();
    let state_missing = read_state(&paths).unwrap();
    assert_eq!(
        state_missing.advanced.lfs.file(&path).unwrap().state,
        LfsFileState::Missing
    );
}

struct Driver {
    paths: RepoPaths,
    state: RepoState,
    runner: GitRunner,
}

impl Driver {
    fn open(paths: RepoPaths, env: Vec<(OsString, OsString)>) -> Self {
        let git = Git::detect("git").unwrap();
        let (state, commands) = open(RepoId {
            root_commit: None,
            remote_url: None,
        });
        let mut driver = Self {
            runner: GitRunner::new(git, paths.clone()).with_env(env),
            paths,
            state,
        };
        for cmd in commands {
            driver.exec(cmd);
        }
        driver
    }

    fn deliver(&mut self, event: Event) {
        let (state, commands) = step(self.state.clone(), event);
        self.state = state;
        for cmd in commands {
            self.exec(cmd);
        }
    }

    fn exec(&mut self, command: CoreCommand) {
        match command {
            CoreCommand::ReadState { read, .. } => {
                self.deliver(read_event(&self.paths, read));
            }
            CoreCommand::RunGit { op, args, expect } => {
                let (tx, rx) = mpsc::channel();
                self.runner.run(op, args, expect, tx);
                loop {
                    let event = rx
                        .recv_timeout(Duration::from_secs(30))
                        .expect("no OpFinished within 30 s");
                    let done = matches!(event, Event::OpFinished(..));
                    self.deliver(event);
                    if done {
                        break;
                    }
                }
            }
            other => panic!("unexpected command in P10 driver: {other:?}"),
        }
    }

    fn outcome(&self) -> Option<OpOutcome> {
        match &self.state.notice {
            Some(Notice::OpEnded { outcome, .. }) => Some(outcome.clone()),
            _ => None,
        }
    }
}

#[test]
fn worktree_add_and_remove_end_to_end_with_lock_and_ref_guards() {
    let sb = Sandbox::new();
    let main = sb.init_repo("wt-e2e");
    sb.commit(&main, "a.txt", b"hello\n", "c1");
    sb.git(&main, &["branch", "existing-br"]);

    let found = discover(&main).unwrap();
    let mut driver = Driver::open(found.paths.clone(), sb.env.clone());

    // 1. Add linked worktree on a new branch.
    let wt_new = sb.root.join("wt-new");
    driver.deliver(Event::WorktreeAddRequested {
        path: wt_new.to_str().unwrap().to_owned(),
        spec: WorktreeSpec::NewBranch {
            name: "wt-branch".into(),
            at: BranchStart::Head,
        },
    });
    assert_eq!(driver.outcome(), Some(OpOutcome::Succeeded));
    assert!(wt_new.join(".git").is_file());
    let disk = driver.state.fresh_disk().unwrap();
    assert_eq!(disk.advanced.worktrees.len(), 2);
    assert!(
        disk.advanced
            .worktrees
            .iter()
            .any(|w| same_worktree_path(&w.path, wt_new.to_str().unwrap()))
    );

    // 2. Lock guard (Law 12): holding `index.lock` refuses the write with PreconditionChanged.
    let wt_blocked = sb.root.join("wt-blocked");
    fs::write(found.paths.git_dir.join("index.lock"), b"lock").unwrap();
    driver.deliver(Event::WorktreeAddRequested {
        path: wt_blocked.to_str().unwrap().to_owned(),
        spec: WorktreeSpec::ExistingBranch(BranchName("existing-br".into())),
    });
    fs::remove_file(found.paths.git_dir.join("index.lock")).unwrap();
    match driver.outcome() {
        Some(OpOutcome::Failed(f)) => assert_eq!(f.class, ErrorClass::PreconditionChanged),
        other => panic!("expected PreconditionChanged with index.lock held, got {other:?}"),
    }
    assert!(!wt_blocked.exists());

    // 3. Remove the linked worktree cleanly (even after locking it with force=true).
    sb.git(&main, &["worktree", "lock", wt_new.to_str().unwrap()]);
    driver.deliver(Event::RefreshRequested);
    driver.deliver(Event::WorktreeRemoveRequested {
        path: wt_new.to_str().unwrap().to_owned(),
        force: true,
    });
    assert_eq!(driver.outcome(), Some(OpOutcome::Succeeded));
    assert!(!wt_new.exists());
    assert_eq!(
        driver.state.fresh_disk().unwrap().advanced.worktrees.len(),
        1
    );
}

#[test]
fn submodule_update_init_recursive_and_sync_work_end_to_end() {
    let sb = Sandbox::new();
    let child = sb.init_repo("sub-child");
    sb.commit(&child, "lib.txt", b"v1\n", "child v1");

    let super_repo = sb.init_repo("sub-super");
    sb.commit(&super_repo, "README.md", b"root\n", "super c1");
    sb.git(
        &super_repo,
        &[
            "submodule",
            "add",
            "-q",
            child.to_str().unwrap(),
            "libs/child",
        ],
    );
    sb.git(&super_repo, &["commit", "-q", "-m", "add child"]);

    // Clone without `--recurse-submodules` so the submodule starts Uninitialized.
    let clone_dir = sb.root.join("sub-clone");
    sb.git(
        &sb.root,
        &[
            "clone",
            "-q",
            super_repo.to_str().unwrap(),
            clone_dir.to_str().unwrap(),
        ],
    );
    let found = discover(&clone_dir).unwrap();
    let mut driver = Driver::open(found.paths.clone(), sb.env.clone());
    assert_eq!(
        driver.state.fresh_disk().unwrap().advanced.submodules[0].state,
        SubmoduleState::Uninitialized
    );

    // Sync then update --init --recursive through the core+shell pipeline.
    driver.deliver(Event::SubmoduleRequested {
        op: SubmoduleOp::SyncRecursive,
        path: None,
    });
    assert_eq!(driver.outcome(), Some(OpOutcome::Succeeded));

    driver.deliver(Event::SubmoduleRequested {
        op: SubmoduleOp::UpdateInitRecursive,
        path: Some(RepoPath("libs/child".into())),
    });
    assert_eq!(driver.outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(
        driver.state.fresh_disk().unwrap().advanced.submodules[0].state,
        SubmoduleState::Clean
    );
    assert!(clone_dir.join("libs/child/lib.txt").is_file());
}

#[test]
fn submodule_update_forces_checkout_and_refuses_dirty_or_on_branch() {
    let sb = Sandbox::new();
    let child = sb.init_repo("sub-child-law9");
    sb.commit(&child, "lib.txt", b"v1\n", "child v1");
    sb.commit(&child, "lib.txt", b"v2\n", "child v2");

    let super_repo = sb.init_repo("sub-super-law9");
    sb.commit(&super_repo, "README.md", b"root\n", "super c1");
    sb.git(
        &super_repo,
        &[
            "submodule",
            "add",
            "-q",
            child.to_str().unwrap(),
            "libs/child",
        ],
    );
    sb.git(&super_repo, &["commit", "-q", "-m", "add child"]);

    let clone_dir = sb.root.join("sub-clone-law9");
    sb.git(
        &sb.root,
        &[
            "clone",
            "-q",
            super_repo.to_str().unwrap(),
            clone_dir.to_str().unwrap(),
        ],
    );
    let found = discover(&clone_dir).unwrap();
    let mut driver = Driver::open(found.paths.clone(), sb.env.clone());

    // Initialize the submodule first (detached HEAD at v2).
    driver.deliver(Event::SubmoduleRequested {
        op: SubmoduleOp::UpdateInitRecursive,
        path: None,
    });
    assert_eq!(driver.outcome(), Some(OpOutcome::Succeeded));

    let sub_dir = clone_dir.join("libs/child");
    let v2_oid = sb.git(&sub_dir, &["rev-parse", "HEAD"]);

    // Configure `submodule.libs/child.update = rebase` to prove `--checkout` overrides it.
    sb.git(&clone_dir, &["config", "submodule.libs/child.update", "rebase"]);
    // Detach at v1 (OutOfSync, clean, detached) -> update must check out v2 cleanly.
    sb.git(&sub_dir, &["checkout", "-q", "--detach", "HEAD~1"]);
    driver.deliver(Event::RefreshRequested);
    driver.deliver(Event::SubmoduleRequested {
        op: SubmoduleOp::UpdateInitRecursive,
        path: Some(RepoPath("libs/child".into())),
    });
    assert_eq!(driver.outcome(), Some(OpOutcome::Succeeded));
    assert_eq!(sb.git(&sub_dir, &["rev-parse", "HEAD"]), v2_oid);

    // 1. Submodule on a local branch (switched AFTER snapshot so shell guard runs too):
    // must be refused and the branch tip must not move (Law 9).
    sb.git(&sub_dir, &["checkout", "-q", "-b", "local-work", "HEAD~1"]);
    let branch_before = sb.git(&sub_dir, &["rev-parse", "refs/heads/local-work"]);
    driver.deliver(Event::SubmoduleRequested {
        op: SubmoduleOp::UpdateInitRecursive,
        path: Some(RepoPath("libs/child".into())),
    });
    match driver.outcome() {
        Some(OpOutcome::Failed(f)) => {
            assert_eq!(f.class, ErrorClass::Expected);
            assert!(
                f.details.contains("local-work"),
                "expected branch name in refusal: {}",
                f.details
            );
        }
        other => panic!("expected shell refusal for on-branch submodule, got {other:?}"),
    }
    assert_eq!(
        sb.git(&sub_dir, &["rev-parse", "refs/heads/local-work"]),
        branch_before
    );
    assert_eq!(
        sb.git(&sub_dir, &["symbolic-ref", "--short", "HEAD"]),
        "local-work"
    );

    // 2. Submodule detached but dirty (dirtied AFTER snapshot so shell guard runs too):
    // must be refused and dirty content preserved.
    sb.git(&sub_dir, &["checkout", "-q", "--detach", "HEAD"]);
    driver.deliver(Event::RefreshRequested);
    fs::write(sub_dir.join("lib.txt"), b"uncommitted work\n").unwrap();
    driver.deliver(Event::SubmoduleRequested {
        op: SubmoduleOp::UpdateInitRecursive,
        path: Some(RepoPath("libs/child".into())),
    });
    match driver.outcome() {
        Some(OpOutcome::Failed(f)) => {
            assert_eq!(f.class, ErrorClass::Expected);
            assert!(
                f.details.contains("uncommitted or untracked"),
                "expected dirty refusal: {}",
                f.details
            );
        }
        other => panic!("expected shell refusal for dirty submodule, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(sub_dir.join("lib.txt")).unwrap(),
        "uncommitted work\n"
    );
}

#[test]
fn submodule_action_uses_literal_pathspec_for_glob_names() {
    let sb = Sandbox::new();
    let child = sb.init_repo("sub-glob-child");
    sb.commit(&child, "lib.txt", b"v1\n", "child v1");

    let super_repo = sb.init_repo("sub-glob-super");
    sb.commit(&super_repo, "README.md", b"root\n", "super c1");
    sb.git(
        &super_repo,
        &[
            "submodule",
            "add",
            "-q",
            child.to_str().unwrap(),
            "libs/a[12]",
        ],
    );
    sb.git(
        &super_repo,
        &["submodule", "add", "-q", child.to_str().unwrap(), "libs/a1"],
    );
    sb.git(&super_repo, &["commit", "-q", "-m", "add glob submodules"]);

    let clone_dir = sb.root.join("sub-glob-clone");
    sb.git(
        &sb.root,
        &[
            "clone",
            "-q",
            super_repo.to_str().unwrap(),
            clone_dir.to_str().unwrap(),
        ],
    );
    let found = discover(&clone_dir).unwrap();
    let mut driver = Driver::open(found.paths.clone(), sb.env.clone());

    // Update ONLY `libs/a[12]` — `libs/a1` must remain Uninitialized.
    driver.deliver(Event::SubmoduleRequested {
        op: SubmoduleOp::UpdateInitRecursive,
        path: Some(RepoPath("libs/a[12]".into())),
    });
    assert_eq!(driver.outcome(), Some(OpOutcome::Succeeded));

    let subs = &driver.state.fresh_disk().unwrap().advanced.submodules;
    let a12 = subs
        .iter()
        .find(|s| s.path == RepoPath("libs/a[12]".into()))
        .unwrap();
    let a1 = subs
        .iter()
        .find(|s| s.path == RepoPath("libs/a1".into()))
        .unwrap();
    assert_eq!(a12.state, SubmoduleState::Clean);
    assert_eq!(a1.state, SubmoduleState::Uninitialized);
    assert!(clone_dir.join("libs/a[12]/lib.txt").is_file());
    assert!(!clone_dir.join("libs/a1/lib.txt").exists());
}

#[test]
fn lfs_pull_reports_environment_error_when_git_lfs_is_missing_and_respects_lock_guard() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("lfs-pull-repo");
    sb.commit(
        &repo,
        ".gitattributes",
        b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        "track lfs",
    );
    sb.commit(
        &repo,
        "asset.bin",
        SAMPLE_LFS_POINTER.as_bytes(),
        "add pointer",
    );

    let found = discover(&repo).unwrap();

    // 1. Simulate a machine where `git-lfs` is NOT installed by pointing PATH
    // to an empty temp directory (while invoking git itself via its full path).
    let empty_bin = sb.root.join("empty-bin");
    fs::create_dir_all(&empty_bin).unwrap();
    let mut no_lfs_env = sb.env.clone();
    no_lfs_env.push((OsString::from("PATH"), empty_bin.into_os_string()));
    // Also override GIT_EXEC_PATH so Git for Windows doesn't find bundled git-lfs.
    no_lfs_env.push((
        OsString::from("GIT_EXEC_PATH"),
        sb.root.clone().into_os_string(),
    ));

    let git = Git::detect("git").unwrap();
    // Resolve absolute git executable path so empty PATH still finds `git` itself.
    let git_exe = which_git();
    let git_abs = Git::detect(git_exe).unwrap_or(git);

    // Verify helper detects absence when PATH/GIT_EXEC_PATH hide git-lfs.
    let lfs_absent = git_lfs_version(&git_abs, &found.paths.workdir, &no_lfs_env).is_none();
    if lfs_absent {
        let mut driver = Driver {
            paths: found.paths.clone(),
            state: {
                let (s, cmds) = open(RepoId {
                    root_commit: None,
                    remote_url: None,
                });
                let CoreCommand::ReadState { read, .. } = cmds[0] else {
                    panic!()
                };
                let (s, _) = step(s, read_event(&found.paths, read));
                s
            },
            runner: GitRunner::new(git_abs.clone(), found.paths.clone()).with_env(no_lfs_env),
        };
        driver.deliver(Event::LfsPullRequested { path: None });
        match driver.outcome() {
            Some(OpOutcome::Failed(f)) => {
                assert_eq!(f.class, ErrorClass::Environment);
                assert!(
                    f.details.contains("Git LFS is not installed"),
                    "{}",
                    f.details
                );
            }
            other => panic!("expected Environment error when git-lfs missing, got {other:?}"),
        }
    }

    // 2. Lock guard (Law 12): holding `index.lock` refuses LfsPull before running.
    let mut driver = Driver::open(found.paths.clone(), sb.env.clone());
    fs::write(found.paths.git_dir.join("index.lock"), b"lock").unwrap();
    driver.deliver(Event::LfsPullRequested { path: None });
    fs::remove_file(found.paths.git_dir.join("index.lock")).unwrap();
    match driver.outcome() {
        Some(OpOutcome::Failed(f)) => assert_eq!(f.class, ErrorClass::PreconditionChanged),
        other => panic!("expected PreconditionChanged with index.lock held, got {other:?}"),
    }
}

fn which_git() -> PathBuf {
    #[cfg(windows)]
    let cmd = ("where", "git");
    #[cfg(not(windows))]
    let cmd = ("which", "git");
    Command::new(cmd.0)
        .arg(cmd.1)
        .output()
        .ok()
        .and_then(|o| {
            o.status.success().then(|| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("git")
                    .trim()
                    .into()
            })
        })
        .unwrap_or_else(|| PathBuf::from("git"))
}
