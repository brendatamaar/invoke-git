//! Choosing which git binary to run: the system git when it is new enough,
//! the MinGit bundled next to the app (Windows installs) only as a fallback.
//!
//! [`resolve`] is pure: the PATH lookup and the version probe are injected,
//! so every case is testable without a real git. The thin IO wrappers live
//! in [`Git::detect_with_fallback`](crate::git::Git::detect_with_fallback).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::git::{GitVersion, SetupError};

/// Folder next to the app executable that holds the extracted MinGit zip.
pub const BUNDLED_DIR: &str = "mingit";

/// Where `git.exe` sits inside the bundled MinGit (`cmd/` is the entry that
/// finds `mingw64/` relative to itself).
pub const BUNDLED_PROGRAM: [&str; 2] = ["cmd", "git.exe"];

/// Which git was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitSource {
    /// `git` found through PATH (or the program the caller named).
    System,
    /// The MinGit shipped in the app's install directory.
    Bundled,
}

/// The chosen binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub program: PathBuf,
    pub version: GitVersion,
    pub source: GitSource,
}

/// The path of the bundled `git.exe` for an app executable at `exe`.
pub fn bundled_path(exe: &Path) -> PathBuf {
    let mut path = exe.parent().unwrap_or(Path::new("")).join(BUNDLED_DIR);
    for part in BUNDLED_PROGRAM {
        path.push(part);
    }
    path
}

/// Name of the git executable searched for on PATH.
pub const PROGRAM_NAME: &str = if cfg!(windows) { "git.exe" } else { "git" };

/// Every `git` on `path_var` (a PATH value), in PATH order, without
/// duplicates and without any inside `app_dir` (the install folder that
/// holds the bundled MinGit): a MinGit someone put on PATH is still the
/// fallback, never the system git. `case_insensitive` is true on Windows.
pub fn system_candidates(
    path_var: &OsStr,
    app_dir: Option<&Path>,
    case_insensitive: bool,
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let key = |path: &Path| -> Vec<String> {
        path.components()
            .map(|part| {
                let text = part.as_os_str().to_string_lossy().into_owned();
                if case_insensitive {
                    text.to_lowercase()
                } else {
                    text
                }
            })
            .collect()
    };
    let app_key = app_dir.map(key);
    let mut seen: Vec<Vec<String>> = Vec::new();
    let mut found = Vec::new();
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let dir_key = key(&dir);
        if app_key.as_ref().is_some_and(|app| dir_key.starts_with(app)) {
            continue;
        }
        let program = dir.join(PROGRAM_NAME);
        if seen.contains(&dir_key) || !exists(&program) {
            continue;
        }
        seen.push(dir_key);
        found.push(program);
    }
    found
}

/// System git first: each candidate in order, and the first one that is new
/// enough wins. Only when none is usable does the bundled git (if `bundled`
/// is given and exists) get a turn. When neither works the error says so for
/// both; the system part is the first candidate's reason.
///
/// `probe` runs `<program> --version` and checks the minimum version;
/// `exists` says whether a file is there.
pub fn resolve(
    system: &[PathBuf],
    bundled: Option<&Path>,
    probe: &dyn Fn(&Path) -> Result<GitVersion, SetupError>,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<Resolved, SetupError> {
    let mut first_error = None;
    for program in system {
        match probe(program) {
            Ok(version) => {
                return Ok(Resolved {
                    program: program.clone(),
                    version,
                    source: GitSource::System,
                });
            }
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    let system_error = first_error
        .unwrap_or_else(|| SetupError::Missing(format!("no {PROGRAM_NAME} found on PATH")));
    let Some(bundled) = bundled.filter(|path| exists(path)) else {
        // No fallback installed: the system git's own reason is the message.
        return Err(system_error);
    };
    match probe(bundled) {
        Ok(version) => Ok(Resolved {
            program: bundled.to_path_buf(),
            version,
            source: GitSource::Bundled,
        }),
        Err(bundled_error) => Err(SetupError::FallbackFailed {
            system: Box::new(system_error),
            bundled: Box::new(bundled_error),
        }),
    }
}
