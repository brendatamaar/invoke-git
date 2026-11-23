//! Incremental commit search. The worker reads from disk and buffers one
//! page, so a slow or cancelled consumer cannot accumulate history.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::Duration;

use invoke_core::search::{SearchField, SearchQuery};

use crate::commit_detail::tree_of;
use crate::history::{self, CommitId};
use crate::precondition::{self, RepoPaths};

/// Maximum number of commits examined (and thus matches emitted) per page.
pub const SCAN_PAGE_SIZE: usize = 256;

#[derive(Debug)]
pub enum SearchEvent {
    /// A batch of matches and scan progress, including batches with no matches.
    Page {
        matches: Vec<CommitId>,
        scanned: usize,
    },
    Done,
    Failed(String),
}

/// Searches the graph's reachable commits on a worker. Set `cancel` to
/// stop; an already queued page may still be received before disconnection.
pub fn search(
    paths: RepoPaths,
    query: SearchQuery,
    cancel: Arc<AtomicBool>,
) -> Receiver<SearchEvent> {
    let (tx, rx) = sync_channel(1);
    let worker_tx = tx.clone();
    let started = thread::Builder::new()
        .name("commit search".into())
        .spawn(move || {
            let result = scan(&paths, &query, &cancel, &worker_tx);
            let event = match result {
                Ok(()) => SearchEvent::Done,
                Err(error) => SearchEvent::Failed(error),
            };
            send(&worker_tx, event, &cancel);
        });
    if let Err(error) = started {
        let _ = tx.try_send(SearchEvent::Failed(format!("start commit search: {error}")));
    }
    rx
}

fn send(tx: &SyncSender<SearchEvent>, mut event: SearchEvent, cancel: &AtomicBool) -> bool {
    while !cancel.load(Ordering::Relaxed) {
        match tx.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(pending)) => {
                event = pending;
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
    false
}

fn scan(
    paths: &RepoPaths,
    query: &SearchQuery,
    cancel: &AtomicBool,
    tx: &SyncSender<SearchEvent>,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    let repo = precondition::open(paths)?;
    let boundary = history::shallow(paths, &repo);
    let mut walk = history::search_walk(&repo, cancel)?;
    loop {
        let mut matches = Vec::new();
        let mut scanned = 0;
        while scanned < SCAN_PAGE_SIZE && !cancel.load(Ordering::Relaxed) {
            let Some(info) = walk.next() else {
                break;
            };
            let info = info.map_err(|e| format!("walk commit history: {e}"))?;
            let id = info.id().detach();
            let matched = if query.field() == SearchField::Hash {
                query.matches_hash(&id.to_string())
            } else {
                let commit = info
                    .object()
                    .map_err(|e| format!("read commit {id}: {e}"))?;
                matches_commit(&repo, &commit, query, boundary.contains(&id))?
            };
            if matched {
                matches.push(id);
            }
            scanned += 1;
        }
        if scanned == 0 || !send(tx, SearchEvent::Page { matches, scanned }, cancel) {
            return Ok(());
        }
    }
}

fn matches_commit(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    query: &SearchQuery,
    shallow_boundary: bool,
) -> Result<bool, String> {
    if let Some(path) = query.path() {
        // The first parent of a shallow boundary is unavailable.
        if shallow_boundary {
            return Ok(false);
        }
        let before = crate::commit_detail::parent_tree(repo, commit)?;
        let after = tree_of(commit)?;
        return Ok(query.matches_path_change(entry(repo, before, path)?, entry(repo, after, path)?));
    }
    if query.field() == SearchField::Message {
        return Ok(query.matches_text(
            &String::from_utf8_lossy(commit.message_raw_sloppy()),
            "",
            "",
        ));
    }
    let author = commit.author().map_err(|e| format!("read author: {e}"))?;
    Ok(query.matches_text(
        "",
        &String::from_utf8_lossy(author.name),
        &String::from_utf8_lossy(author.email),
    ))
}

fn entry<'repo>(
    repo: &'repo gix::Repository,
    mut tree: gix::Tree<'repo>,
    path: &str,
) -> Result<Option<(CommitId, gix::object::tree::EntryMode)>, String> {
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        let Some(entry) = tree.find_entry(component) else {
            return Ok(None);
        };
        let (id, mode) = (entry.object_id(), entry.mode());
        if components.peek().is_none() {
            return Ok(Some((id, mode)));
        }
        // A file or gitlink cannot contain a Git path. In particular the
        // object referenced by a gitlink may not exist in this repository.
        if !mode.is_tree() {
            return Ok(None);
        }
        tree = repo
            .find_tree(id)
            .map_err(|e| format!("look up {path}: {e}"))?;
    }
    Ok(None)
}
