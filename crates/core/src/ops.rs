//! Long operations: what was asked for, how it is going, how it ended.

use std::fmt;

use crate::event::RefValue;
use crate::ids::{BranchName, Oid, RepoPath};

/// A write the user asked for, waiting in or running from the op queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteRequest {
    Commit {
        message: String,
        amend: bool,
    },
    /// Reverse the newest undo point.
    Undo,
    // Task 15: branches. Checked against the fresh snapshot at dispatch.
    CreateBranch {
        name: BranchName,
        at: BranchStart,
        /// Also make it the current branch (`git switch -c`).
        switch: bool,
    },
    SwitchBranch {
        name: BranchName,
    },
    RenameBranch {
        from: BranchName,
        to: BranchName,
    },
    DeleteBranch {
        name: BranchName,
        /// Delete even if the branch is not fully merged.
        force: bool,
    },
}

impl WriteRequest {
    /// Writes that take an undo point before they run.
    pub fn takes_undo_point(&self) -> bool {
        !matches!(self, WriteRequest::Undo)
    }
}

/// Where a new branch starts (task 15).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchStart {
    /// The commit `HEAD` is on now, which is also how a detached `HEAD` is
    /// left by creating a branch.
    Head,
    Commit(Oid),
}

/// Typed git invocation. The shell turns it into argv (task 4); keeping it
/// typed here lets tests check what the core asked for without parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitArgs {
    Commit {
        message: String,
        amend: bool,
    },
    // Task 15: branches. Each carries the ref values it expects; the shell
    // refuses the run when disk differs (see `GitArgs::expected_refs`).
    CreateBranch {
        name: BranchName,
        at: Oid,
        switch: bool,
    },
    SwitchBranch {
        name: BranchName,
        /// Where the branch is now.
        tip: Oid,
    },
    RenameBranch {
        from: BranchName,
        /// Where `from` is now.
        tip: Oid,
        to: BranchName,
    },
    DeleteBranch {
        name: BranchName,
        /// Where the branch is now.
        tip: Oid,
        force: bool,
    },
}

impl GitArgs {
    /// The refs this run expects to find, besides `HEAD` and the index
    /// (rule 5). `oid: None` means the ref must not exist.
    pub fn expected_refs(&self) -> Vec<RefValue> {
        let at = |name: &BranchName, oid: Option<&Oid>| RefValue {
            name: name.to_ref(),
            oid: oid.cloned(),
        };
        match self {
            GitArgs::Commit { .. } => Vec::new(),
            GitArgs::CreateBranch { name, .. } => vec![at(name, None)],
            GitArgs::SwitchBranch { name, tip } | GitArgs::DeleteBranch { name, tip, .. } => {
                vec![at(name, Some(tip))]
            }
            GitArgs::RenameBranch { from, tip, to } => {
                vec![at(from, Some(tip)), at(to, None)]
            }
        }
    }
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
