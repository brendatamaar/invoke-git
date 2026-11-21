//! Long operations: what was asked for, how it is going, how it ended.

use std::fmt;

use crate::ids::{IndexId, RepoPath};

/// A write the user asked for, waiting in or running from the op queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteRequest {
    Commit {
        message: String,
        amend: bool,
    },
    /// Reverse the newest undo point.
    Undo,
    // Task 11: index writes from the status tree.
    Stage(StageRequest),
    Unstage(StageRequest),
}

/// Typed git invocation. The shell turns it into argv (task 4); keeping it
/// typed here lets tests check what the core asked for without parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitArgs {
    Commit { message: String, amend: bool },
}

/// Progress parsed from git's stderr, e.g. `Receiving objects: 45% (9/20)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub stage: String,
    pub done: u64,
    pub total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthPrompt {
    pub kind: AuthKind,
    /// The prompt text git or ssh printed.
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthKind {
    Username,
    Password,
    Passphrase,
    HostKey,
}

/// The user's answer to an [`AuthPrompt`]. It never appears in debug output.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthAnswer(pub String);

impl fmt::Debug for AuthAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AuthAnswer(<redacted>)")
    }
}

/// How an operation ended, as classified by the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpOutcome {
    Succeeded,
    Failed(Failure),
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub class: ErrorClass,
    /// Raw git stderr, shown only behind a "Details" toggle.
    pub details: String,
}

/// The error classes from `docs/spec/core-design.md` → "Errors".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// Rejected push, merge conflict, nothing to commit.
    Expected,
    /// Index or ref moved since we read it, or `index.lock` is held.
    PreconditionChanged,
    Auth,
    Environment,
    Network,
    Bug,
}

/// Which part of the stage/unstage UI a request covers. Task 11 refines it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    File(RepoPath),
    Hunk {
        path: RepoPath,
        hunk: usize,
    },
    Lines {
        path: RepoPath,
        hunk: usize,
        lines: Vec<usize>,
    },
}

impl Selection {
    pub fn path(&self) -> &RepoPath {
        match self {
            Selection::File(path)
            | Selection::Hunk { path, .. }
            | Selection::Lines { path, .. } => path,
        }
    }

    /// What an undo point says: "Stage a.rs", "Unstage hunk 2 in a.rs",
    /// "Stage 3 lines in a.rs".
    pub fn label(&self, verb: &str) -> String {
        match self {
            Selection::File(path) => format!("{verb} {}", path.0),
            Selection::Hunk { path, hunk } => format!("{verb} hunk {} in {}", hunk + 1, path.0),
            Selection::Lines { path, lines, .. } => {
                let n = lines.len();
                let noun = if n == 1 { "line" } else { "lines" };
                format!("{verb} {n} {noun} in {}", path.0)
            }
        }
    }
}

/// A unified-diff patch for `git apply --cached` (built by `diff::build_patch`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch(pub String);

/// A stage or unstage the user asked for (task 11).
///
/// The UI builds the patch from the diff it shows, so the core carries it
/// and never needs file contents. `based_on` is the index id the diff was
/// computed against: the write is refused, and the diff must be refreshed,
/// unless the index on disk is still exactly that one (rule 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageRequest {
    /// What was selected; names the undo point and the notice.
    pub selection: Selection,
    pub patch: Patch,
    pub based_on: IndexId,
}
