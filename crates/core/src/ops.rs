//! Long operations: what was asked for, how it is going, how it ended.

use std::fmt;

use crate::conflict::{Operation, ResolveChoice, Side};
use crate::event::{HeadTarget, RefValue};
use crate::ids::{BranchName, IndexId, Oid, RefName, RepoPath, TagName};
use crate::message::CommitMessage;

/// A write the user asked for, waiting in or running from the op queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteRequest {
    Commit {
        /// Cleaned by the core when the request arrives (task 14).
        message: CommitMessage,
        amend: bool,
    },
    /// Reverse the newest undo point.
    Undo,
    InteractiveRebase {
        plan: crate::RebasePlan,
        head: crate::Head,
    },
    // Task 11: index writes from the status tree.
    Stage(StageRequest),
    Unstage(StageRequest),
    // Task 13: takes lines out of a worktree file; undoable.
    Discard(DiscardRequest),
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
    // P3: tags. Checked against the fresh snapshot at dispatch.
    CreateTag {
        name: TagName,
        at: BranchStart,
        /// `Some` makes it an annotated tag with this message.
        message: Option<CommitMessage>,
    },
    DeleteTag {
        name: TagName,
    },
    /// Sends one local tag to a remote. Irreversible (law 9).
    PushTag {
        remote: String,
        name: TagName,
    },
    /// Deletes a tag on a remote (`git push <remote> :refs/tags/<name>`).
    /// Irreversible (law 9); the local tag is untouched.
    DeleteRemoteTag {
        remote: String,
        name: TagName,
    },
    // Task 20: stash. Apply, pop and drop name the stash by commit id.
    StashSave {
        message: String,
        include_untracked: bool,
    },
    StashApply {
        stash: Oid,
    },
    StashPop {
        stash: Oid,
    },
    StashDrop {
        stash: Oid,
    },
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
    // Task 23: the conflict flow. Each names the operation the user saw
    // paused; it is checked against the fresh snapshot when it runs.
    ResolveFile {
        path: RepoPath,
        choice: ResolveChoice,
    },
    Continue {
        operation: Operation,
    },
    Skip {
        operation: Operation,
    },
    Abort {
        operation: Operation,
    },
    // P2: start a single-commit cherry-pick or revert. `onto` is the kind of
    // `HEAD` the user saw; the request is refused if `HEAD` is another
    // branch by the time it runs (law 9).
    CherryPick {
        commit: Oid,
        onto: HeadTarget,
    },
    Revert {
        commit: Oid,
        onto: HeadTarget,
    },
}

impl WriteRequest {
    /// Writes that take an undo point before they run. Fetch moves only
    /// remote-tracking refs and set-upstream only config: nothing an undo
    /// could restore (task 17). Task 23: only Continue leaves a marker.
    /// While files are conflicted the index cannot be written out as a tree,
    /// so resolving, skipping and aborting have nothing to save; abort is
    /// itself git's exact way back.
    pub fn takes_undo_point(&self) -> bool {
        !matches!(
            self,
            WriteRequest::Undo
                | WriteRequest::Fetch { .. }
                | WriteRequest::SetUpstream { .. }
                | WriteRequest::ResolveFile { .. }
                | WriteRequest::Skip { .. }
                | WriteRequest::Abort { .. }
        )
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
    // P3: tags. Each carries the tag ref value it expects (see
    // `GitArgs::expected_refs`).
    CreateTag {
        name: TagName,
        at: Oid,
        /// Cleaned message of an annotated tag.
        message: Option<String>,
    },
    DeleteTag {
        name: TagName,
        /// What the tag ref holds now (the tag object when annotated).
        tip: Oid,
    },
    PushTag {
        remote: String,
        name: TagName,
        /// What the local tag ref holds now.
        tip: Oid,
    },
    DeleteRemoteTag {
        remote: String,
        name: TagName,
    },
    // Task 20: stash. `index` is the position (`stash@{index}`) the core
    // found `stash` at; the shell checks that the entry there still has that
    // id and refuses otherwise (rule 5).
    StashSave {
        message: String,
        include_untracked: bool,
    },
    StashApply {
        index: usize,
        stash: Oid,
    },
    StashPop {
        index: usize,
        stash: Oid,
    },
    StashDrop {
        index: usize,
        stash: Oid,
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
    // Task 23. `path` is always one literal path (no globbing). Take one
    // side whole: check it out of the index stage, then stage it.
    ResolveTakeSide {
        path: RepoPath,
        side: Side,
    },
    /// Resolve by deleting the file (the side chosen has none).
    ResolveRemove {
        path: RepoPath,
    },
    /// Stage the worktree file as it is.
    ResolveStage {
        path: RepoPath,
    },
    Continue {
        operation: Operation,
    },
    /// Skip the commit a rebase stopped on; `operation` is always a rebase.
    Skip {
        operation: Operation,
    },
    Abort {
        operation: Operation,
    },
    // P2. The commit is an object id, so it can never be read as an option.
    CherryPick {
        commit: Oid,
    },
    Revert {
        commit: Oid,
    },
    InteractiveRebase(crate::RebasePlan),
    GuardedRebase {
        control: crate::rebase::RebaseControl,
        step: u32,
        total: u32,
        origin: crate::RebaseOrigin,
    },
}

impl GitArgs {
    /// The stash entry this run expects to find, as (position, commit id).
    pub fn stash_target(&self) -> Option<(usize, &Oid)> {
        match self {
            GitArgs::StashApply { index, stash }
            | GitArgs::StashPop { index, stash }
            | GitArgs::StashDrop { index, stash } => Some((*index, stash)),
            _ => None,
        }
    }

    /// The refs this run expects to find, besides `HEAD` and the index
    /// (rule 5). `oid: None` means the ref must not exist.
    pub fn expected_refs(&self) -> Vec<RefValue> {
        let at = |name: &BranchName, oid: Option<&Oid>| RefValue {
            name: name.to_ref(),
            oid: oid.cloned(),
        };
        let tag_at = |name: &TagName, oid: Option<&Oid>| RefValue {
            name: name.to_ref(),
            oid: oid.cloned(),
        };
        match self {
            GitArgs::Commit { .. }
            | GitArgs::StashSave { .. }
            | GitArgs::StashApply { .. }
            | GitArgs::StashPop { .. }
            | GitArgs::StashDrop { .. }
            // Task 17: the lease and `HEAD` are what these expect.
            | GitArgs::Fetch { .. }
            | GitArgs::Pull { .. }
            | GitArgs::Push { .. }
            | GitArgs::SetUpstream { .. }
            // Task 23: the paused operation and the index are what these
            // expect (`Expect`), checked by the shell.
            | GitArgs::ResolveTakeSide { .. }
            | GitArgs::ResolveRemove { .. }
            | GitArgs::ResolveStage { .. }
            | GitArgs::Continue { .. }
            | GitArgs::Skip { .. }
            | GitArgs::Abort { .. }
            // P2: `HEAD` and the index are what these expect.
            | GitArgs::CherryPick { .. }
            | GitArgs::Revert { .. }
            | GitArgs::InteractiveRebase(_) => Vec::new(),
            GitArgs::GuardedRebase { origin, .. } => origin
                .branch
                .iter()
                .map(|branch| at(branch, Some(&origin.tip)))
                .collect(),
            GitArgs::CreateBranch { name, .. } => vec![at(name, None)],
            GitArgs::CreateTag { name, .. } => vec![tag_at(name, None)],
            GitArgs::DeleteTag { name, tip } | GitArgs::PushTag { name, tip, .. } => {
                vec![tag_at(name, Some(tip))]
            }
            GitArgs::DeleteRemoteTag { .. } => Vec::new(),
            GitArgs::SwitchBranch { name, tip } | GitArgs::DeleteBranch { name, tip, .. } => {
                vec![at(name, Some(tip))]
            }
            GitArgs::RenameBranch { from, tip, to } => {
                vec![at(from, Some(tip)), at(to, None)]
            }
        }
    }

    /// Task 23: runs git in a way that may open the commit message editor
    /// (continuing commits with the prepared message). The shell sets
    /// `GIT_EDITOR=true` for these so git never waits for an editor.
    pub fn may_open_editor(&self) -> bool {
        matches!(
            self,
            GitArgs::Continue { .. }
                | GitArgs::Skip { .. }
                | GitArgs::Abort { .. }
                | GitArgs::CherryPick { .. }
                | GitArgs::Revert { .. }
                | GitArgs::InteractiveRebase(_)
                | GitArgs::GuardedRebase { .. }
        )
    }

    /// Talks to a remote, so it may ask for credentials and shows progress.
    pub fn is_network(&self) -> bool {
        matches!(
            self,
            GitArgs::Fetch { .. }
                | GitArgs::Pull { .. }
                | GitArgs::Push { .. }
                | GitArgs::PushTag { .. }
                | GitArgs::DeleteRemoteTag { .. }
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
    /// git ran fine and refused, with its own message to show: a commit
    /// hook that exited non-zero (task 14, `tasks/004.DONE.md` question 3).
    /// The UI shows the output inline; it is not a bug.
    Rejected,
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

/// A discard the user asked for (task 13): selected changes are taken out of
/// one worktree file. The patch is built by `diff::build_patch` with
/// `Direction::Discard` from the diff index to worktree the user sees.
///
/// Discarding loses work, so the request carries both things it expects to
/// find (rule 5): the index the diff was made against and the exact bytes of
/// the worktree file it was made from. It is refused when either moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscardRequest {
    /// What was selected; names the undo point and the notice.
    pub selection: Selection,
    pub patch: Patch,
    /// The index id the diff was made against.
    pub based_on: IndexId,
    /// The git blob id of the worktree file's bytes the diff was made from
    /// (`None`: the file did not exist, so the discard brings it back).
    pub expect_file: Option<Oid>,
}
