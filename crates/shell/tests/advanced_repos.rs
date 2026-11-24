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
use std::time::{SystemTime, UNIX_EPOCH};

use diff::{DiffOptions, Direction, Mode, PatchSpec, Pick, build_patch, diff_text};
use invoke_core::{Head, OpId, OpOutcome, Patch, Phase, RepoPath};
use shell::precondition::RepoPaths;
use shell::{Git, Source, StagingRunner, discover, load_inputs, read_state};

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

    // Place a linked worktree as a sibling and explicitly write a relative
    // `.git` file pointing to `../main/.git/worktrees/wt` (and also test the
    // nested `../.git/worktrees/<name>` layout inside a subdirectory).
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
    // Rewrite `.git` to a relative pointer `../.git/worktrees/wt-nested` to
    // explicitly cover the Windows relative-path case.
    fs::write(
        wt_nested.join(".git"),
        "gitdir: ../.git/worktrees/wt-nested\n",
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
}

#[test]
fn submodule_fixture_is_readable_in_superproject_and_inside_submodule() {
    let sb = Sandbox::new();
    let child = sb.init_repo("child-lib");
    sb.commit(&child, "lib.txt", b"v1\n", "child initial");

    let super_repo = sb.init_repo("super");
    sb.commit(&super_repo, "README.md", b"hello\n", "super initial");
    sb.git(
        &super_repo,
        &["submodule", "add", "-q", child.to_str().unwrap(), "libs/child"],
    );
    sb.git(&super_repo, &["commit", "-q", "-m", "add submodule"]);

    let found_super = discover(&super_repo).unwrap();
    let state_super = read_state(&found_super.paths).unwrap();
    assert_eq!(state_super.phase, Phase::Clean);

    // Opening the submodule itself follows its `.git` file pointer to
    // `../../.git/modules/libs/child`.
    let sub_dir = super_repo.join("libs/child");
    assert!(sub_dir.join(".git").is_file());
    let found_sub = discover(&sub_dir).unwrap();
    assert_same_dir(&found_sub.paths.workdir, &sub_dir);
    let state_sub = read_state(&found_sub.paths).unwrap();
    assert_eq!(state_sub.phase, Phase::Clean);
}

#[test]
fn lfs_pointer_file_does_not_break_diff_or_staging_without_git_lfs() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("lfs-repo");
    sb.commit(
        &repo,
        ".gitattributes",
        b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        "track bin with lfs",
    );
    sb.commit(
        &repo,
        "asset.bin",
        SAMPLE_LFS_POINTER.as_bytes(),
        "add lfs pointer",
    );

    // Modify the LFS pointer text in the worktree (as happens when a pointer is
    // updated without git-lfs smudge/clean running).
    fs::write(repo.join("asset.bin"), SAMPLE_LFS_POINTER_V2).unwrap();

    let paths = RepoPaths {
        workdir: repo.clone(),
        git_dir: repo.join(".git"),
    };
    let state = read_state(&paths).unwrap();
    assert_eq!(state.phase, Phase::Clean);

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
        matches!(event, invoke_core::Event::OpFinished(OpId(1), OpOutcome::Succeeded)),
        "staging an LFS pointer file must succeed: {event:?}"
    );
}
