//! Finding a repo from any path inside it.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::precondition::RepoPaths;

/// A discovered repo: where its worktree and git dirs are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    /// `workdir`, plus the git dir that holds this worktree's own `HEAD`,
    /// `index` and in-progress markers.
    pub paths: RepoPaths,
    /// The shared git dir (objects, packed refs, config). Equal to
    /// `paths.git_dir` except in a linked worktree.
    pub common_dir: PathBuf,
}

#[derive(Debug)]
pub enum DiscoverError {
    /// No repo contains the path (or it cannot be opened).
    NotARepo { path: PathBuf, reason: String },
    /// The repo has no working tree. Invoke Git works on checkouts only.
    Bare { git_dir: PathBuf },
}

impl fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoverError::NotARepo { path, reason } => {
                write!(
                    f,
                    "{} is not inside a git repository ({reason})",
                    path.display()
                )
            }
            DiscoverError::Bare { git_dir } => write!(
                f,
                "{} is a bare repository; open a folder with a working tree instead",
                git_dir.display()
            ),
        }
    }
}

impl std::error::Error for DiscoverError {}

/// Walks up from `start` to the repo that contains it. `start` may be the
/// worktree root, any folder below it, or the git dir itself. Handles `.git`
/// being a file (linked worktrees and submodules) and refuses bare repos.
pub fn discover(start: &Path) -> Result<Discovered, DiscoverError> {
    let not_a_repo = |reason: String| DiscoverError::NotARepo {
        path: start.to_owned(),
        reason,
    };
    let repo = gix::discover(start).map_err(|error| not_a_repo(error.to_string()))?;
    let absolute = |path: &Path| std::path::absolute(path).map_err(|e| not_a_repo(e.to_string()));
    let git_dir = absolute(repo.git_dir())?;
    let Some(workdir) = repo.workdir() else {
        return Err(DiscoverError::Bare { git_dir });
    };
    Ok(Discovered {
        paths: RepoPaths {
            workdir: absolute(workdir)?,
            git_dir,
        },
        common_dir: absolute(repo.common_dir())?,
    })
}
