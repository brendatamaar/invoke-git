//! The user's own git config (task 11, spec question 3): the shell ignores
//! it by default so tests are the same everywhere, and honours it once the
//! app asks (`shell::use_user_config(true)`). Git for Windows' default
//! `core.autocrlf=true` sits in exactly that place.
//!
//! This is the only test in its binary, because it sets process-wide
//! environment variables and the config switch.

use std::fs;
use std::process::Command;

use invoke_core::{FileChange, RepoPath};
use shell::precondition::RepoPaths;
use shell::{Source, load_inputs, read::read_state, use_user_config};

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn the_users_autocrlf_is_ignored_by_default_and_honoured_when_asked() {
    let root = std::env::temp_dir().join(format!("invoke-git-userconfig-{}", std::process::id()));
    let workdir = root.join("repo");
    fs::create_dir_all(&workdir).unwrap();
    let global = root.join("gitconfig");
    fs::write(&global, "[core]\n\tautocrlf = true\n").unwrap();
    // SAFETY: the only test in this binary, and nothing has started threads
    // that read the environment yet.
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", &global);
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_AUTHOR_NAME", "Test");
        std::env::set_var("GIT_AUTHOR_EMAIL", "test@example.com");
        std::env::set_var("GIT_COMMITTER_NAME", "Test");
        std::env::set_var("GIT_COMMITTER_EMAIL", "test@example.com");
    }
    git(&workdir, &["init", "-q", "-b", "main"]);
    // What Git for Windows does: the blob is LF, the checkout is CRLF.
    fs::write(workdir.join("a.txt"), b"one\r\ntwo\r\nthree\r\n").unwrap();
    git(&workdir, &["add", "-A"]);
    git(&workdir, &["commit", "-q", "-m", "c"]);
    fs::write(workdir.join("a.txt"), b"one\r\ntwo\r\nthree\r\n").unwrap();
    assert_eq!(git(&workdir, &["status", "--porcelain"]), "");

    let paths = RepoPaths {
        git_dir: workdir.join(".git"),
        workdir: workdir.clone(),
    };
    let status = |paths: &RepoPaths| {
        read_state(paths)
            .unwrap()
            .status
            .entries
            .into_iter()
            .map(|e| (e.path.0, e.staged, e.unstaged))
            .collect::<Vec<_>>()
    };

    // Default: isolated, like every other test. The file looks changed
    // because the setting is not seen. This pins the default.
    use_user_config(false);
    assert_eq!(
        status(&paths),
        vec![("a.txt".to_owned(), None, Some(FileChange::Modified))]
    );

    // Asked: the same answer as git.
    use_user_config(true);
    assert_eq!(status(&paths), vec![]);
    fs::write(workdir.join("a.txt"), b"one\r\nTWO\r\nthree\r\n").unwrap();
    let inputs = load_inputs(&paths, &RepoPath("a.txt".into()), Source::IndexToWorktree).unwrap();
    assert_eq!(inputs.old.as_deref(), Some("one\ntwo\nthree\n"));
    assert_eq!(inputs.new.as_deref(), Some("one\nTWO\nthree\n"));

    use_user_config(false);
    let _ = fs::remove_dir_all(&root);
}
