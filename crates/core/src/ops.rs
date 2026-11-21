//! Long operations: what was asked for, how it is going, how it ended.

use std::fmt;

use crate::ids::RepoPath;

/// A write the user asked for, waiting in or running from the op queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteRequest {
    Commit { message: String, amend: bool },
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

/// A unified-diff patch for `git apply --cached`. Task 11 builds these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Patch(pub String);
