//! Long operations: what was asked for, how it is going, how it ended.

use std::fmt;

use crate::ids::{BranchName, Oid, RefName, RepoPath};

/// A write the user asked for, waiting in or running from the op queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteRequest {
    Commit {
        message: String,
        amend: bool,
    },
    /// Reverse the newest undo point.
    Undo,
    // Task 17: network and branch-config writes.
    /// Moves remote-tracking refs only (law 9); never a local branch.
    Fetch {
        remote: String,
    },
    /// Fetch, then merge or rebase the current branch onto `from`.
    Pull {
        /// The branch checked out when the user asked.
        local: BranchName,
        from: RemoteBranch,
        mode: PullMode,
    },
    Push {
        /// The branch checked out when the user asked; it is what is pushed.
        local: BranchName,
        to: RemoteBranch,
        /// `Some` makes it a force push, leased against the remote value the
        /// user reviewed (law 10). There is no force push without one.
        lease: Option<Lease>,
        set_upstream: bool,
    },
    SetUpstream {
        local: BranchName,
        upstream: RemoteBranch,
    },
}

/// A branch on a remote, such as `origin` and `main`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteBranch {
    pub remote: String,
    pub branch: BranchName,
}

impl RemoteBranch {
    /// Our last-known copy: `refs/remotes/<remote>/<branch>`.
    pub fn tracking_ref(&self) -> RefName {
        RefName(format!("refs/remotes/{}/{}", self.remote, self.branch.0))
    }

    /// The branch as the remote names it: `refs/heads/<branch>`.
    pub fn remote_ref(&self) -> RefName {
        self.branch.to_ref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullMode {
    Merge,
    Rebase,
}

/// `--force-with-lease=<ref>:<expected>`: the push is refused by the remote
/// unless `remote_ref` still holds `expected`. Both parts are mandatory, so
/// a bare `--force-with-lease` (which trusts whatever was fetched last) and
/// a plain `--force` cannot be expressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub remote_ref: RefName,
    pub expected: Oid,
}

/// Typed git invocation. The shell turns it into argv (task 4); keeping it
/// typed here lets tests check what the core asked for without parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitArgs {
    Commit {
        message: String,
        amend: bool,
    },
    // Task 17.
    Fetch {
        remote: String,
    },
    Pull {
        from: RemoteBranch,
        mode: PullMode,
    },
    Push {
        local: BranchName,
        to: RemoteBranch,
        lease: Option<Lease>,
        set_upstream: bool,
    },
    SetUpstream {
        local: BranchName,
        upstream: RemoteBranch,
    },
}

impl GitArgs {
    /// Talks to a remote, so it may ask for credentials and shows progress.
    pub fn is_network(&self) -> bool {
        matches!(
            self,
            GitArgs::Fetch { .. } | GitArgs::Pull { .. } | GitArgs::Push { .. }
        )
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
