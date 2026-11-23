//! `Command::ReadState`: a fresh read of everything in [`DiskState`] that
//! lives in `.git`, through gix and plain file reads (never the git CLI,
//! never `index.lock`). The decisions are the core's; this only gathers.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use invoke_core::detect::{Markers, RebaseFiles, detect_phase};
use invoke_core::message;
use invoke_core::{
    BranchName, Command, ConflictKind, Conflicts, DiskState, ErrorClass, Event, Failure, LockState,
    Oid, Phase, ReadId, RefName, RefsSnapshot, RepoPath, StashEntry, TagName, Tracking, Upstream,
};

use crate::precondition::{self, RepoPaths};
use crate::resolve_undo;
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
    let markers = read_markers(&paths.git_dir);
    // Task 23: the resolved files are only looked up while a merge, rebase,
    // cherry-pick or revert is in progress.
    let in_progress = detect_phase(&markers, Conflicts::default()) != Phase::Clean;
    let conflicts = read_conflicts(&repo, &index_path, in_progress).map_err(fail)?;
    let phase = detect_phase(&markers, conflicts);
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

/// Task 14: the message of the commit `HEAD` is on, cleaned, as the amend
/// editor's starting text. `None` on an unborn branch (nothing to amend).
/// Read through gix; takes no lock.
pub fn head_message(paths: &RepoPaths) -> Result<Option<String>, Failure> {
    let fail = |details: String| Failure {
        class: ErrorClass::Bug,
        details,
    };
    let repo = precondition::open(paths).map_err(fail)?;
    if repo
        .head()
        .map_err(|error| fail(format!("read HEAD: {error}")))?
        .is_unborn()
    {
        return Ok(None);
    }
    let commit = repo
        .head_commit()
        .map_err(|error| fail(format!("read the HEAD commit: {error}")))?;
    let raw = commit
        .message_raw()
        .map_err(|error| fail(format!("read the HEAD commit message: {error}")))?;
    Ok(Some(message::normalize(&String::from_utf8_lossy(raw))))
}

/// Task 14: the message git prepared for the merge in progress
/// (`MERGE_MSG`), without the `#` lines it adds as instructions, as the
/// starting text for concluding the merge. `None` when there is none.
pub fn merge_message(paths: &RepoPaths) -> Result<Option<String>, Failure> {
    match fs::read(paths.git_dir.join("MERGE_MSG")) {
        Ok(bytes) => Ok(Some(message::strip_comment_lines(
            &String::from_utf8_lossy(&bytes),
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Failure {
            class: ErrorClass::Bug,
            details: format!("read MERGE_MSG: {error}"),
        }),
    }
}

/// Which markers exist in the worktree's own git dir, with their counters.
pub(crate) fn read_markers(git_dir: &Path) -> Markers {
    let rebase = |dir: &str, step: &str, total: &str| {
        let dir = git_dir.join(dir);
        dir.is_dir().then(|| RebaseFiles {
            step: fs::read_to_string(dir.join(step)).ok(),
            total: fs::read_to_string(dir.join(total)).ok(),
            edit: dir.join("amend").is_file(),
            origin: rebase_origin(&dir),
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

fn rebase_origin(dir: &Path) -> Option<invoke_core::RebaseOrigin> {
    let tip = Oid::from_hex(fs::read_to_string(dir.join("orig-head")).ok()?.trim()).ok()?;
    let head = fs::read_to_string(dir.join("head-name")).ok()?;
    let branch = match head.trim() {
        "detached HEAD" => None,
        name => Some(BranchName::parse(name.strip_prefix("refs/heads/")?).ok()?),
    };
    Some(invoke_core::RebaseOrigin { branch, tip })
}

/// The conflicted files (task 23). Paths with an index entry at stage 1, 2
/// or 3 are Unresolved, and the stages present give the kind. Paths git
/// remembers as resolved (the index's resolve-undo record) are Resolved. The
/// record is read only while something is in progress: with nothing paused
/// the core drops the list anyway.
fn read_conflicts(
    repo: &gix::Repository,
    index_path: &Path,
    in_progress: bool,
) -> Result<Conflicts, String> {
    if !index_path.exists() {
        return Ok(Conflicts::default());
    }
    let index = repo
        .open_index()
        .map_err(|error| format!("read index entries: {error}"))?;
    // Per path: which of stages 1, 2 and 3 exist.
    let mut stages: BTreeMap<RepoPath, [bool; 3]> = BTreeMap::new();
    for entry in index.entries() {
        let stage = entry.stage_raw();
        if stage == 0 {
            continue;
        }
        let seen = stages
            .entry(RepoPath(entry.path(&index).to_string()))
            .or_default();
        if let Some(slot) = seen.get_mut(usize::try_from(stage - 1).unwrap_or(usize::MAX)) {
            *slot = true;
        }
    }
    let unresolved = stages
        .into_iter()
        .filter_map(|(path, [base, ours, theirs])| {
            ConflictKind::from_stages(base, ours, theirs).map(|kind| (path, kind))
        });
    let resolved = if in_progress {
        let bytes = fs::read(index_path).map_err(|error| format!("read index: {error}"))?;
        resolve_undo::resolved_paths(&bytes, repo.object_hash().len_in_bytes())
    } else {
        Vec::new()
    };
    Ok(Conflicts::from_parts(
        unresolved,
        resolved.into_iter().map(RepoPath),
    ))
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
    let peeled_tags = read_peeled_tags(repo, &refs)?;
    Ok(RefsSnapshot {
        refs,
        upstreams,
        peeled_tags,
    })
}

/// P3: for each annotated tag (a tag ref that names a tag object, not a
/// commit), the commit it finally names. Lightweight tags are left out.
fn read_peeled_tags(
    repo: &gix::Repository,
    refs: &BTreeMap<RefName, Oid>,
) -> Result<BTreeMap<TagName, Oid>, String> {
    let mut peeled = BTreeMap::new();
    for (name, tip) in refs {
        let Some(short) = name.0.strip_prefix("refs/tags/") else {
            continue;
        };
        let id = gix::ObjectId::from_hex(tip.as_hex().as_bytes())
            .map_err(|error| format!("{name:?}: {error}"))?;
        let object = repo
            .find_object(id)
            .map_err(|error| format!("read tag {short}: {error}"))?;
        if object.kind != gix::object::Kind::Tag {
            continue;
        }
        // Annotated tags may point to a blob or tree (e.g. a PGP key tag);
        // only commit targets are recorded for graph badges.
        let Ok(target) = object.peel_to_kind(gix::object::Kind::Commit) else {
            continue;
        };
        peeled.insert(
            TagName(short.to_owned()),
            precondition::oid(&target.id().detach())?,
        );
    }
    Ok(peeled)
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
