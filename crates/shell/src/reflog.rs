//! Reading local reflogs (`HEAD` and local branches) through `gix` (P7).
//!
//! Purely read-only from `.git/logs/` (and the common git dir for linked
//! worktrees); never locks `.git` and never writes or edits reflog files.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread;

use invoke_core::{BranchName, ReflogEntry, ReflogTarget};

use crate::precondition::{self, RepoPaths};

/// Result delivered by [`read_reflog_async`]. If cancelled, the channel closes
/// without sending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReflogEvent {
    Loaded {
        target: ReflogTarget,
        entries: Vec<ReflogEntry>,
    },
    Failed {
        target: ReflogTarget,
        error: String,
    },
}

/// Reads the local reflog for `target` (`HEAD` or a local branch), newest
/// first (`index == 0` is `@{0}`), optionally capped to `limit` entries.
///
/// Returns an empty list when the repository has no commits yet (unborn
/// `HEAD`), when the branch does not exist, or when reflogs are absent or
/// disabled (`core.logAllRefUpdates = false`).
///
/// The log is read forward and reversed (same as `read_stashes`): `gix`'s
/// reverse iterator scans a fixed buffer window that could truncate long
/// messages.
pub fn read_reflog(
    paths: &RepoPaths,
    target: &ReflogTarget,
    limit: Option<usize>,
) -> Result<Vec<ReflogEntry>, String> {
    let repo = precondition::open(paths)?;
    let mut entries = match target {
        ReflogTarget::Head => {
            let head = repo.head().map_err(|error| format!("read HEAD: {error}"))?;
            let mut log = head.log_iter();
            let lines = log
                .all()
                .map_err(|error| format!("read HEAD reflog: {error}"))?;
            collect_lines(lines)?
        }
        ReflogTarget::Branch(name) => {
            let full = name.to_ref();
            let Some(reference) = repo
                .try_find_reference(full.0.as_str())
                .map_err(|error| format!("read {}: {error}", full.0))?
            else {
                return Ok(Vec::new());
            };
            let mut log = reference.log_iter();
            let lines = log
                .all()
                .map_err(|error| format!("read {} reflog: {error}", full.0))?;
            collect_lines(lines)?
        }
    };

    entries.reverse();
    if let Some(limit) = limit {
        entries.truncate(limit);
    }
    for (index, entry) in entries.iter_mut().enumerate() {
        entry.index = index;
        entry.selector = target.selector(index);
    }
    Ok(entries)
}

fn collect_lines(
    lines: Option<gix::refs::file::log::iter::Forward<'_>>,
) -> Result<Vec<ReflogEntry>, String> {
    let mut entries = Vec::new();
    for line in lines.into_iter().flatten() {
        let line = line.map_err(|error| format!("parse reflog line: {error}"))?;
        let previous = line.previous_oid();
        let old_oid = if previous.is_null() {
            None
        } else {
            Some(precondition::oid(&previous)?)
        };
        let new_oid = precondition::oid(&line.new_oid())?;
        let time = line
            .signature
            .time()
            .map_err(|error| format!("parse reflog timestamp: {error}"))?;
        entries.push(ReflogEntry {
            index: 0,
            selector: String::new(),
            old_oid,
            new_oid,
            message: line.message.to_string(),
            actor: line.signature.name.to_string(),
            email: line.signature.email.to_string(),
            seconds: time.seconds,
            offset: time.offset,
        });
    }
    Ok(entries)
}

/// Lists the reflog targets available in the repository: `HEAD` first,
/// followed by every local branch sorted by name.
pub fn list_reflog_targets(paths: &RepoPaths) -> Result<Vec<ReflogTarget>, String> {
    let repo = precondition::open(paths)?;
    let platform = repo
        .references()
        .map_err(|error| format!("list references: {error}"))?;
    let branches = platform
        .prefixed("refs/heads/")
        .map_err(|error| format!("list local branches: {error}"))?;
    let mut names = BTreeSet::new();
    for reference in branches {
        let reference = reference.map_err(|error| format!("read local branch: {error}"))?;
        let full = reference.name().as_bstr().to_string();
        if let Some(short) = full.strip_prefix("refs/heads/")
            && let Ok(parsed) = BranchName::parse(short)
        {
            names.insert(parsed);
        }
    }
    let mut targets = Vec::with_capacity(1 + names.len());
    targets.push(ReflogTarget::Head);
    targets.extend(names.into_iter().map(ReflogTarget::Branch));
    Ok(targets)
}

/// Reads a reflog on a background thread so UI panes stay responsive. Setting
/// `cancel` suppresses delivery of the result.
pub fn read_reflog_async(
    paths: RepoPaths,
    target: ReflogTarget,
    limit: Option<usize>,
    cancel: Arc<AtomicBool>,
) -> Receiver<ReflogEvent> {
    let (tx, rx) = channel();
    let spawned = thread::Builder::new().name("reflog read".into()).spawn({
        let tx = tx.clone();
        let target = target.clone();
        move || {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let result = read_reflog(&paths, &target, limit);
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let event = match result {
                Ok(entries) => ReflogEvent::Loaded { target, entries },
                Err(error) => ReflogEvent::Failed { target, error },
            };
            let _ = tx.send(event);
        }
    });
    if let Err(error) = spawned {
        let _ = tx.send(ReflogEvent::Failed {
            target,
            error: format!("start reflog read: {error}"),
        });
    }
    rx
}
