//! `Command::ReadState`: a fresh read of everything in [`DiskState`] that
//! lives in `.git`, through gix and plain file reads (never the git CLI,
//! never `index.lock`). The decisions are the core's; this only gathers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use invoke_core::detect::{Markers, RebaseFiles, detect_phase};
use invoke_core::{
    BranchName, Command, Conflicts, DiskState, ErrorClass, Event, Failure, LockState, Oid, ReadId,
    RefName, RefsSnapshot, RepoPath, StashEntry, Tracking, Upstream,
};

use crate::precondition::{self, RepoPaths};
use crate::status::read_status;

/// Runs a `ReadState` command and returns the event that answers it. Other
/// commands are not reads and give `None`. The scope is ignored: every read
/// is a whole [`DiskState`] (the core replaces its snapshot wholesale).
pub fn execute(paths: &RepoPaths, command: &Command) -> Option<Event> {
    match command {
        Command::ReadState { read, .. } => Some(read_event(paths, *read)),
        _ => None,
    }
}

/// `StateRead` on success, `ReadFailed` otherwise.
pub fn read_event(paths: &RepoPaths, read: ReadId) -> Event {
    match read_state(paths) {
        Ok(disk) => Event::StateRead { read, disk },
        Err(failure) => Event::ReadFailed { read, failure },
    }
}

/// Reads head, phase (with conflicts), index id, refs and lock from disk.
/// `status` is the sorted status list (task 11).
pub fn read_state(paths: &RepoPaths) -> Result<DiskState, Failure> {
    let fail = |details: String| Failure {
        class: ErrorClass::Bug,
        details,
    };
    let repo = precondition::open(paths).map_err(fail)?;
    let head = precondition::read_head(&repo).map_err(fail)?;
    let index_path = paths.git_dir.join("index");
    let index = precondition::index_id(&index_path, repo.object_hash().len_in_bytes())
        .map_err(|error| fail(format!("read index: {error}")))?;
    let conflicts = read_conflicts(&repo, &index_path).map_err(fail)?;
    let phase = detect_phase(&read_markers(&paths.git_dir), conflicts);
    let refs = read_refs(&repo).map_err(fail)?;
    let status = precondition::open_with_worktree(paths)
        .and_then(|worktree| read_status(&worktree))
        .map_err(fail)?;
    let stashes = read_stashes(&repo).map_err(fail)?;
    let lock = if paths.git_dir.join("index.lock").exists() {
        LockState::HeldByOther
    } else {
        LockState::Free
    };
    Ok(DiskState {
        head,
        phase,
        index,
        status,
        refs,
        stashes,
        lock,
    })
}

/// Which markers exist in the worktree's own git dir, with their counters.
fn read_markers(git_dir: &Path) -> Markers {
    let rebase = |dir: &str, step: &str, total: &str| {
        let dir = git_dir.join(dir);
        dir.is_dir().then(|| RebaseFiles {
            step: fs::read_to_string(dir.join(step)).ok(),
            total: fs::read_to_string(dir.join(total)).ok(),
        })
    };
    Markers {
        rebase_merge: rebase("rebase-merge", "msgnum", "end"),
        rebase_apply: rebase("rebase-apply", "next", "last"),
        merge_head: git_dir.join("MERGE_HEAD").exists(),
        cherry_pick_head: git_dir.join("CHERRY_PICK_HEAD").exists(),
        revert_head: git_dir.join("REVERT_HEAD").exists(),
    }
}

/// Paths with an index entry at stage 1, 2 or 3, sorted and unique.
fn read_conflicts(repo: &gix::Repository, index_path: &Path) -> Result<Conflicts, String> {
    if !index_path.exists() {
        return Ok(Conflicts::default());
    }
    let index = repo
        .open_index()
        .map_err(|error| format!("read index entries: {error}"))?;
    let paths: BTreeSet<RepoPath> = index
        .entries()
        .iter()
        .filter(|entry| entry.stage_raw() != 0)
        .map(|entry| RepoPath(entry.path(&index).to_string()))
        .collect();
    Ok(Conflicts {
        paths: paths.into_iter().collect(),
    })
}

/// The stash list: the reflog of `refs/stash`, newest first, so index 0 is
/// `stash@{0}`. Read through gix, never `git stash list`.
///
/// The log is read forward and reversed: the reverse reader works through a
/// fixed window that a long message could cut.
pub(crate) fn read_stashes(repo: &gix::Repository) -> Result<Vec<StashEntry>, String> {
    let Some(reference) = repo
        .try_find_reference("refs/stash")
        .map_err(|error| format!("read refs/stash: {error}"))?
    else {
        return Ok(Vec::new());
    };
    let mut log = reference.log_iter();
    let lines = log
        .all()
        .map_err(|error| format!("read stash reflog: {error}"))?;
    let mut entries = Vec::new();
    for line in lines.into_iter().flatten() {
        let line = line.map_err(|error| format!("parse stash reflog: {error}"))?;
        let id = Oid::from_hex(&line.new_oid.to_string())
            .map_err(|error| format!("stash reflog id: {error:?}"))?;
        entries.push(StashEntry {
            id,
            message: line.message.to_string(),
        });
    }
    entries.reverse();
    Ok(entries)
}

/// Every ref under `refs/`, loose and packed, with the object it names.
/// Symbolic refs (such as `refs/remotes/origin/HEAD`) give their target's id;
/// a dangling one is left out.
fn read_refs(repo: &gix::Repository) -> Result<RefsSnapshot, String> {
    let platform = repo
        .references()
        .map_err(|error| format!("read refs: {error}"))?;
    let all = platform
        .all()
        .map_err(|error| format!("read refs: {error}"))?;
    let mut refs = BTreeMap::new();
    for reference in all {
        let mut reference = reference.map_err(|error| format!("read ref: {error}"))?;
        let name = reference.name().as_bstr().to_string();
        let id = match reference.try_id() {
            Some(id) => id.detach(),
            None => match reference.peel_to_id() {
                Ok(id) => id.detach(),
                Err(_) => continue,
            },
        };
        refs.insert(RefName(name), precondition::oid(&id)?);
    }
    let upstreams = read_upstreams(repo, &refs)?;
    Ok(RefsSnapshot { refs, upstreams })
}

/// Each local branch's configured upstream and how far apart the two are.
/// Branches without one are left out. The counts are commits only on the
/// branch (ahead) and only on the upstream (behind).
fn read_upstreams(
    repo: &gix::Repository,
    refs: &BTreeMap<RefName, invoke_core::Oid>,
) -> Result<BTreeMap<BranchName, Upstream>, String> {
    let mut upstreams = BTreeMap::new();
    for (name, tip) in refs {
        let Some(short) = name.0.strip_prefix("refs/heads/") else {
            continue;
        };
        let Ok(full) = gix::refs::FullName::try_from(name.0.as_str()) else {
            continue;
        };
        let Some(Ok(tracking)) =
            repo.branch_remote_tracking_ref_name(full.as_ref(), gix::remote::Direction::Fetch)
        else {
            continue;
        };
        let upstream_ref = RefName(tracking.as_bstr().to_string());
        let counts = match refs.get(&upstream_ref) {
            None => Tracking::Gone,
            Some(up) => {
                let tip = gix::ObjectId::from_hex(tip.as_hex().as_bytes())
                    .map_err(|error| format!("{name:?}: {error}"))?;
                let up = gix::ObjectId::from_hex(up.as_hex().as_bytes())
                    .map_err(|error| format!("{upstream_ref:?}: {error}"))?;
                Tracking::Apart {
                    ahead: count_only_on(repo, tip, up)?,
                    behind: count_only_on(repo, up, tip)?,
                }
            }
        };
        upstreams.insert(
            BranchName(short.to_owned()),
            Upstream {
                name: upstream_ref,
                tracking: counts,
            },
        );
    }
    Ok(upstreams)
}

/// Commits reachable from `tip` but not from `other`.
fn count_only_on(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    other: gix::ObjectId,
) -> Result<u32, String> {
    if tip == other {
        return Ok(0);
    }
    let walk = repo
        .rev_walk([tip])
        .with_hidden([other])
        .all()
        .map_err(|error| format!("walk history: {error}"))?;
    let mut count = 0u32;
    for commit in walk {
        commit.map_err(|error| format!("walk history: {error}"))?;
        count = count.saturating_add(1);
    }
    Ok(count)
}
