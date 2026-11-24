//! P11: which git binary is used. System git >= 2.40 always wins; the
//! bundled MinGit is only a fallback; with neither the error says so.
//! Nothing here touches the network or a real repository.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use shell::git::{Git, GitVersion, MIN_VERSION, SetupError};
use shell::git_discovery::{GitSource, PROGRAM_NAME, bundled_path, resolve, system_candidates};

const V: fn(u32, u32, u32) -> GitVersion = |major, minor, patch| GitVersion {
    major,
    minor,
    patch,
};

fn system() -> PathBuf {
    PathBuf::from("git")
}

fn bundled() -> PathBuf {
    PathBuf::from("app/mingit/cmd/git.exe")
}

/// Runs `resolve` with canned probe answers and records which programs were
/// probed, in order.
fn run(
    system_answer: Result<GitVersion, SetupError>,
    bundled_answer: Option<Result<GitVersion, SetupError>>,
) -> (
    Result<shell::git_discovery::Resolved, SetupError>,
    Vec<PathBuf>,
) {
    let probed = RefCell::new(Vec::new());
    let (sys, bun) = (system(), bundled());
    let result = resolve(
        std::slice::from_ref(&sys),
        Some(&bun),
        &|program: &Path| {
            probed.borrow_mut().push(program.to_path_buf());
            if program == system() {
                system_answer.clone()
            } else {
                bundled_answer.clone().expect("bundled probed but absent")
            }
        },
        &|path: &Path| bundled_answer.is_some() && path == bundled(),
    );
    (result, probed.into_inner())
}

#[test]
fn valid_system_git_wins_and_the_bundled_one_is_never_probed() {
    let (result, probed) = run(Ok(V(2, 45, 1)), Some(Ok(V(2, 50, 0))));
    let resolved = result.unwrap();
    assert_eq!(resolved.source, GitSource::System);
    assert_eq!(resolved.program, system());
    assert_eq!(resolved.version, V(2, 45, 1));
    assert_eq!(probed, vec![system()]);
}

#[test]
fn old_system_git_falls_back_to_the_bundled_one() {
    let (result, _) = run(Err(SetupError::TooOld(V(2, 39, 3))), Some(Ok(V(2, 47, 0))));
    let resolved = result.unwrap();
    assert_eq!(resolved.source, GitSource::Bundled);
    assert_eq!(resolved.program, bundled());
}

#[test]
fn missing_system_git_falls_back_to_the_bundled_one() {
    let (result, _) = run(
        Err(SetupError::Missing("git: not found".into())),
        Some(Ok(V(2, 47, 0))),
    );
    assert_eq!(result.unwrap().source, GitSource::Bundled);
}

#[test]
fn no_system_git_and_no_bundled_one_reports_the_system_problem() {
    let (result, probed) = run(Err(SetupError::Missing("git: not found".into())), None);
    assert_eq!(
        result.unwrap_err(),
        SetupError::Missing("git: not found".into())
    );
    assert_eq!(
        probed,
        vec![system()],
        "a file that is not there is not run"
    );
}

#[test]
fn old_system_git_and_no_bundled_one_reports_the_old_version() {
    let (result, _) = run(Err(SetupError::TooOld(V(2, 30, 1))), None);
    assert_eq!(result.unwrap_err(), SetupError::TooOld(V(2, 30, 1)));
}

#[test]
fn a_bundled_git_that_does_not_work_reports_both_problems() {
    let (result, _) = run(
        Err(SetupError::TooOld(V(2, 30, 1))),
        Some(Err(SetupError::Missing("broken".into()))),
    );
    assert_eq!(
        result.unwrap_err(),
        SetupError::FallbackFailed {
            system: Box::new(SetupError::TooOld(V(2, 30, 1))),
            bundled: Box::new(SetupError::Missing("broken".into())),
        }
    );
}

fn path_var(dirs: &[&Path]) -> std::ffi::OsString {
    std::env::join_paths(dirs).unwrap()
}

#[test]
fn an_old_git_first_on_path_does_not_hide_a_new_one_later() {
    let (old, new) = (PathBuf::from("old/git"), PathBuf::from("new/git"));
    let resolved = resolve(
        &[old.clone(), new.clone()],
        Some(&bundled()),
        &|program: &Path| {
            if program == old {
                Err(SetupError::TooOld(V(2, 39, 3)))
            } else if program == new {
                Ok(V(2, 45, 0))
            } else {
                panic!("bundled git probed while a system git works")
            }
        },
        &|_| true,
    )
    .unwrap();
    assert_eq!(resolved.source, GitSource::System);
    assert_eq!(resolved.program, new);
}

#[test]
fn no_git_on_path_and_no_bundled_one_is_missing() {
    let error = resolve(&[], None, &|_| panic!("nothing to probe"), &|_| false).unwrap_err();
    assert!(matches!(error, SetupError::Missing(_)), "{error:?}");
}

#[test]
fn the_apps_own_mingit_on_path_is_not_a_system_git() {
    let app = Path::new("C:/Apps/Invoke Git");
    let mingit_cmd = app.join("mingit").join("cmd");
    let system = Path::new("C:/Program Files/Git/cmd");
    let var = path_var(&[&mingit_cmd, system]);
    let found = system_candidates(&var, Some(app), false, &|_| true);
    assert_eq!(found, vec![system.join(PROGRAM_NAME)]);
}

#[test]
fn app_folder_match_ignores_case_when_asked() {
    let app = Path::new("C:/Apps/Invoke Git");
    let upper = Path::new("C:/APPS/INVOKE GIT/mingit/cmd");
    let var = path_var(&[upper]);
    assert!(system_candidates(&var, Some(app), true, &|_| true).is_empty());
    assert_eq!(
        system_candidates(&var, Some(app), false, &|_| true).len(),
        1
    );
}

#[test]
fn candidates_keep_path_order_skip_missing_and_duplicates() {
    let (a, b, c) = (Path::new("a"), Path::new("b"), Path::new("c"));
    let var = path_var(&[a, b, a, c]);
    let missing = b.join(PROGRAM_NAME);
    let found = system_candidates(&var, None, false, &|p| p != missing);
    assert_eq!(found, vec![a.join(PROGRAM_NAME), c.join(PROGRAM_NAME)]);
}

#[test]
fn the_bundled_path_sits_next_to_the_executable() {
    let path = bundled_path(Path::new("C:/Apps/Invoke Git/invoke-git.exe"));
    let parts: Vec<_> = path.components().rev().take(3).collect();
    assert_eq!(parts[0].as_os_str(), "git.exe");
    assert_eq!(parts[1].as_os_str(), "cmd");
    assert_eq!(parts[2].as_os_str(), "mingit");
}

proptest! {
    /// Any system git at or above the minimum is chosen, whatever the
    /// bundled one is.
    #[test]
    fn new_enough_system_git_is_always_chosen(
        major in 2u32..4, minor in 0u32..120, patch in 0u32..20, bundled_ok in any::<bool>(),
    ) {
        let version = V(major, minor, patch);
        prop_assume!(version >= MIN_VERSION);
        let bundled_answer = if bundled_ok { Ok(V(2, 99, 0)) } else { Err(SetupError::Missing("x".into())) };
        let (result, probed) = run(Ok(version), Some(bundled_answer));
        prop_assert_eq!(result.unwrap().source, GitSource::System);
        prop_assert_eq!(probed.len(), 1);
    }

    /// Any system failure with a working bundled git ends on the bundled git.
    #[test]
    fn any_system_failure_ends_on_the_bundled_git(
        major in 0u32..3, minor in 0u32..40, patch in 0u32..20, missing in any::<bool>(),
    ) {
        let error = if missing {
            SetupError::Missing("m".into())
        } else {
            SetupError::TooOld(V(major.min(2), minor, patch))
        };
        let (result, _) = run(Err(error), Some(Ok(V(2, 47, 0))));
        prop_assert_eq!(result.unwrap().source, GitSource::Bundled);
    }
}

/// A fake `git --version` program. Windows runs a `.cmd`, Unix a shell script.
#[cfg(windows)]
fn fake_git(dir: &Path, name: &str, version: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{name}.cmd"));
    fs::write(
        &path,
        format!(
            "@echo off
echo git version {version}
"
        ),
    )
    .unwrap();
    path
}

#[cfg(unix)]
fn fake_git(dir: &Path, name: &str, version: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(
        &path,
        format!(
            "#!/bin/sh
echo 'git version {version}'
"
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Temp(PathBuf);
impl Temp {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("invoke-git-disc-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The real probe (`git --version` through a process) end to end.
#[test]
fn real_probe_prefers_system_then_falls_back_to_bundled() {
    let temp = Temp::new("real");
    let new = fake_git(&temp.0.join("sys-new"), "git", "2.45.1.windows.1");
    let old = fake_git(&temp.0.join("sys-old"), "git", "2.39.2");
    let mingit = fake_git(&temp.0.join("mingit"), "git", "2.47.1.windows.1");
    let nowhere = temp.0.join("nope").join("git");

    let git = Git::detect_with_fallback_at(&new, Some(&mingit)).unwrap();
    assert_eq!(git.source(), GitSource::System);
    assert_eq!(git.version(), V(2, 45, 1));

    let git = Git::detect_with_fallback_at(&old, Some(&mingit)).unwrap();
    assert_eq!(git.source(), GitSource::Bundled);

    let git = Git::detect_with_fallback_at(&nowhere, Some(&mingit)).unwrap();
    assert_eq!(git.source(), GitSource::Bundled);

    let error = Git::detect_with_fallback_at(&nowhere, Some(&nowhere)).unwrap_err();
    assert!(matches!(error, SetupError::Missing(_)), "{error:?}");

    let error = Git::detect_with_fallback_at(&old, None).unwrap_err();
    assert_eq!(error, SetupError::TooOld(V(2, 39, 2)));
}
