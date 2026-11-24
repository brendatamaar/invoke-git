//! Choosing which git binary to run: the system git when it is new enough,
//! the MinGit bundled next to the app (Windows installs) only as a fallback.
//!
//! [`resolve`] is pure: the PATH lookup and the version probe are injected,
//! so every case is testable without a real git. The thin IO wrappers live
//! in [`Git::detect_with_fallback`](crate::git::Git::detect_with_fallback).

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

/// System git first. Only when it is absent, unusable or older than the
/// minimum does the bundled git (if `bundled` is given and exists) get a
/// turn. When neither works the error says so for both.
///
/// `probe` runs `<program> --version` and checks the minimum version;
/// `exists` says whether a file is there.
pub fn resolve(
    system: &Path,
    bundled: Option<&Path>,
    probe: &dyn Fn(&Path) -> Result<GitVersion, SetupError>,
    exists: &dyn Fn(&Path) -> bool,
) -> Result<Resolved, SetupError> {
    let system_error = match probe(system) {
        Ok(version) => {
            return Ok(Resolved {
                program: system.to_path_buf(),
                version,
                source: GitSource::System,
            });
        }
        Err(error) => error,
    };
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
