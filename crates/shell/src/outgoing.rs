//! What a push would send (task 21), read through gix: the commits that are
//! on the checked-out branch and not on its upstream, and the files they
//! change together (`git diff @{u}...HEAD`). A branch without an upstream
//! (or whose upstream is gone) is compared with what the remotes already
//! have: every commit that no remote-tracking branch contains.
//!
//! The per-commit diffs are `commit_detail`'s; this adds the combined one.

use invoke_core::RepoPath;

use crate::commit_detail::{self, ChangedFile, CommitSides};
use crate::history::CommitId;
use crate::precondition::{self, RepoPaths};
use crate::staging::InputsError;

/// More than this and the list says it stopped (the review is for a
/// handful of commits; a thousand is already a different kind of push).
pub const MAX_COMMITS: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingCommit {
    pub id: String,
    pub subject: String,
    /// Author time and the author's UTC offset, in seconds.
    pub seconds: i64,
    pub offset: i32,
    pub is_merge: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// The tip that would be pushed.
    pub head: String,
    /// The exact target and excluded commits used by this comparison.
    pub upstream_tip: Option<String>,
    pub excluded_tips: Vec<String>,
    /// The commit the combined diff starts from: where the branch left what
    /// the remote has. `None` compares with the empty tree.
    pub base: Option<String>,
    /// Newest first.
    pub commits: Vec<OutgoingCommit>,
    /// More commits exist than [`MAX_COMMITS`].
    pub truncated: bool,
    /// What changed between `base` and `head`, sorted by path.
    pub files: Vec<ChangedFile>,
}

fn tips_of(repo: &gix::Repository, prefix: &str) -> Result<Vec<gix::ObjectId>, String> {
    let platform = repo.references().map_err(|e| format!("read refs: {e}"))?;
    let refs = platform
        .prefixed(prefix)
        .map_err(|e| format!("read refs: {e}"))?;
    let mut tips = Vec::new();
    for reference in refs {
        let mut reference = reference.map_err(|e| format!("read ref: {e}"))?;
        if let Ok(id) = reference.peel_to_id() {
            tips.push(id.detach());
        }
    }
    Ok(tips)
}

/// Reads the outgoing commits of `HEAD` against `upstream` (a full ref name
/// such as `refs/remotes/origin/main`), or against every remote-tracking
/// branch when there is none or it no longer exists.
pub fn read_outgoing(paths: &RepoPaths, upstream: Option<&str>) -> Result<Outgoing, String> {
    use gix::revision::walk::Sorting;
    use gix::traverse::commit::simple::CommitTimeOrder;
    let repo = precondition::open(paths)?;
    let head = repo
        .head_id()
        .map_err(|e| format!("there is no commit to push: {e}"))?
        .detach();
    let upstream_tip = match upstream {
        Some(name) => repo
            .try_find_reference(name)
            .map_err(|e| format!("read {name}: {e}"))?
            .and_then(|mut reference| reference.peel_to_id().ok())
            .map(|id| id.detach()),
        None => None,
    };
    let hidden = match upstream_tip {
        Some(tip) => vec![tip],
        None => tips_of(&repo, "refs/remotes/")?,
    };
    let walk = repo
        .rev_walk([head])
        .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
        .with_hidden(hidden.clone())
        .all()
        .map_err(|e| format!("walk history: {e}"))?;
    let mut commits = Vec::new();
    let mut truncated = false;
    let mut oldest_parent: Option<gix::ObjectId> = None;
    for info in walk {
        let info = info.map_err(|e| format!("walk history: {e}"))?;
        if commits.len() == MAX_COMMITS {
            truncated = true;
            break;
        }
        let commit = repo
            .find_commit(info.id)
            .map_err(|e| format!("read commit: {e}"))?;
        let message = commit.message_raw_sloppy();
        let subject = message
            .split(|&b| b == b'\n')
            .next()
            .map(|line| String::from_utf8_lossy(line).trim_end().to_owned())
            .unwrap_or_default();
        let author = commit.author().map_err(|e| format!("read author: {e}"))?;
        let time = author.time().map_err(|e| format!("read time: {e}"))?;
        oldest_parent = commit.parent_ids().next().map(|id| id.detach());
        commits.push(OutgoingCommit {
            id: info.id.to_string(),
            subject,
            seconds: time.seconds,
            offset: time.offset,
            is_merge: commit.parent_ids().count() > 1,
        });
    }
    // With an upstream the combined diff starts where the branch left it
    // (`@{u}...HEAD`). Without one it starts at the parent of the oldest
    // outgoing commit, which for a branch cut from the remote's branch is
    // the fork point.
    let base = match upstream_tip {
        Some(tip) => repo
            .merge_base(head, tip)
            .map_err(|e| format!("find the merge base: {e}"))?
            .map(|id| id.detach()),
        None if truncated => None,
        None => oldest_parent,
    };
    let new_tree = repo
        .find_commit(head)
        .map_err(|e| format!("read commit: {e}"))?
        .tree()
        .map_err(|e| format!("read tree: {e}"))?;
    let old_tree = match base {
        Some(base) => repo
            .find_commit(base)
            .map_err(|e| format!("read commit: {e}"))?
            .tree()
            .map_err(|e| format!("read tree: {e}"))?,
        None => repo.empty_tree(),
    };
    let files = commit_detail::changed_files(&repo, &old_tree, &new_tree)?;
    let mut excluded_tips: Vec<_> = hidden.into_iter().map(|id| id.to_string()).collect();
    excluded_tips.sort_unstable();
    excluded_tips.dedup();
    Ok(Outgoing {
        head: head.to_string(),
        upstream_tip: upstream_tip.map(|id| id.to_string()),
        excluded_tips,
        base: base.map(|id| id.to_string()),
        commits,
        truncated,
        files,
    })
}

/// The two sides of `path` for the combined diff: at `base` (`None`: the
/// empty tree) and at `head`.
pub fn read_range_sides(
    paths: &RepoPaths,
    base: Option<&CommitId>,
    head: &CommitId,
    path: &RepoPath,
) -> Result<CommitSides, InputsError> {
    let read = InputsError::Read;
    let repo = precondition::open(paths).map_err(read)?;
    let tree_at = |id: &CommitId| -> Result<_, InputsError> {
        repo.find_commit(*id)
            .map_err(|e| InputsError::Read(format!("read commit {id}: {e}")))?
            .tree()
            .map_err(|e| InputsError::Read(format!("read tree: {e}")))
    };
    let new_tree = tree_at(head)?;
    let old_tree = match base {
        Some(base) => tree_at(base)?,
        None => repo.empty_tree(),
    };
    Ok(CommitSides {
        old: commit_detail::side(&old_tree, path)?,
        new: commit_detail::side(&new_tree, path)?,
    })
}
