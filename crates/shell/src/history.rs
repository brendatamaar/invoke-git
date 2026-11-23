//! Commit history for the graph (backlog task 18): the walk, and the lazy
//! commit metadata. Read through gix only; no lock is taken.
//!
//! The walk starts from `HEAD`, every local and remote branch and every tag
//! (spec: "Starting points"), in topological order (children before
//! parents), using the commit-graph file when there is one. It sends the
//! commits in pages of [`graph::PAGE_SIZE`] so the layout can run on each
//! page as it arrives. A shallow clone's boundary commits are flagged
//! `truncated`.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use gix::traverse::commit::topo::{Builder, Sorting};

use crate::precondition::{self, RepoPaths};

/// A commit id the graph can copy and compare cheaply.
pub use gix::ObjectId as CommitId;

/// What the walk reports. The last event is `Done` or `Failed`, unless the
/// walk was cancelled (then the channel just closes).
pub enum HistoryEvent {
    /// The walk began; `commit_graph` says whether the commit-graph file
    /// was used (without it the walk is slower; the UI may offer to write
    /// one).
    Started {
        commit_graph: bool,
    },
    Page(Vec<graph::Commit<CommitId>>),
    Done {
        commits: usize,
    },
    Failed(String),
}

/// Starts the walk on its own thread. Set `cancel` to stop it (a refresh
/// replaces the walk). `limit` caps the number of commits (for tests and
/// for huge repos while the UI is still being tuned).
pub fn walk(
    paths: RepoPaths,
    limit: Option<usize>,
    cancel: Arc<AtomicBool>,
) -> Receiver<HistoryEvent> {
    let (tx, rx) = channel();
    let spawned = thread::Builder::new().name("history walk".into()).spawn({
        let tx = tx.clone();
        move || {
            if let Err(error) = run(&paths, limit, &cancel, &tx) {
                let _ = tx.send(HistoryEvent::Failed(error));
            }
        }
    });
    if let Err(error) = spawned {
        let _ = tx.send(HistoryEvent::Failed(format!("start the walk: {error}")));
    }
    rx
}

/// The commits that start the walk: `HEAD` and every ref under
/// `refs/heads`, `refs/remotes` and `refs/tags` that points (after peeling
/// tags) at a commit. Sorted and without duplicates, so the same repo
/// always walks the same way.
fn tips(repo: &gix::Repository) -> Result<Vec<CommitId>, String> {
    let mut tips: HashSet<CommitId> = HashSet::new();
    if let Ok(id) = repo.head_id() {
        tips.insert(id.detach());
    }
    let platform = repo
        .references()
        .map_err(|e| format!("list references: {e}"))?;
    let all = platform
        .all()
        .map_err(|e| format!("list references: {e}"))?;
    for reference in all {
        let Ok(mut reference) = reference else {
            continue;
        };
        let name = reference.name().as_bstr().to_string();
        let wanted = ["refs/heads/", "refs/remotes/", "refs/tags/"]
            .iter()
            .any(|prefix| name.starts_with(prefix));
        if !wanted {
            continue;
        }
        let Ok(id) = reference.peel_to_id() else {
            continue;
        };
        let is_commit = repo
            .find_object(id.detach())
            .is_ok_and(|object| object.kind == gix::object::Kind::Commit);
        if is_commit {
            tips.insert(id.detach());
        }
    }
    let mut tips: Vec<CommitId> = tips.into_iter().collect();
    tips.sort();
    Ok(tips)
}

/// Search shares the graph's roots and commit-graph acceleration. A
/// breadth-first walk can yield immediately even without a commit-graph.
pub(crate) fn search_walk<'repo>(
    repo: &'repo gix::Repository,
    boundary: &'repo HashSet<CommitId>,
    cancel: &'repo AtomicBool,
) -> Result<Box<dyn Iterator<Item = Result<CommitId, String>> + 'repo>, String> {
    let tips = tips(repo)?;
    if boundary.is_empty() {
        let walk = repo
            .rev_walk(tips)
            .sorting(gix::revision::walk::Sorting::BreadthFirst)
            .selected(move |_| !cancel.load(Ordering::Relaxed))
            .map_err(|e| format!("start commit search walk: {e}"))?;
        return Ok(Box::new(walk.map(|info| {
            info.map(|info| info.id().detach())
                .map_err(|e| format!("walk commit history: {e}"))
        })));
    }

    // Cut just the shallow commit's parent edges. The same parents may
    // still be reachable from other tips through non-shallow edges.
    let mut seen: HashSet<_> = tips.iter().copied().collect();
    let mut pending = VecDeque::from(tips);
    Ok(Box::new(std::iter::from_fn(move || {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let id = pending.pop_front()?;
        if !boundary.contains(&id) {
            match repo.find_commit(id) {
                Ok(commit) => pending.extend(
                    commit
                        .parent_ids()
                        .map(|parent| parent.detach())
                        .filter(|parent| seen.insert(*parent)),
                ),
                Err(error) => return Some(Err(format!("read commit {id}: {error}"))),
            }
        }
        Some(Ok(id))
    })))
}

/// The shallow boundary: the commits listed in `.git/shallow` (in a linked
/// worktree, the common dir's file).
pub(crate) fn shallow(paths: &RepoPaths, repo: &gix::Repository) -> HashSet<CommitId> {
    let file = repo.common_dir().join("shallow");
    let text = fs::read_to_string(&file)
        .or_else(|_| fs::read_to_string(paths.git_dir.join("shallow")))
        .unwrap_or_default();
    text.lines()
        .filter_map(|line| CommitId::from_hex(line.trim().as_bytes()).ok())
        .collect()
}

/// Orders the commits reachable from `tips` topologically (children first,
/// newest first among the ready ones), treating `boundary` commits as roots.
fn shallow_order(
    repo: &gix::Repository,
    tips: &[CommitId],
    boundary: &HashSet<CommitId>,
) -> Result<Vec<graph::Commit<CommitId>>, String> {
    use std::collections::{BinaryHeap, HashMap};
    let mut known: HashMap<CommitId, (Vec<CommitId>, i64)> = HashMap::new();
    let mut stack: Vec<CommitId> = tips.to_vec();
    while let Some(id) = stack.pop() {
        if known.contains_key(&id) {
            continue;
        }
        let commit = repo
            .find_commit(id)
            .map_err(|e| format!("read commit {id}: {e}"))?;
        let seconds = commit.time().map(|t| t.seconds).unwrap_or(0);
        let parents: Vec<CommitId> = if boundary.contains(&id) {
            Vec::new()
        } else {
            commit.parent_ids().map(|p| p.detach()).collect()
        };
        stack.extend(parents.iter().copied());
        known.insert(id, (parents, seconds));
    }
    let mut children: HashMap<CommitId, usize> = known.keys().map(|id| (*id, 0)).collect();
    for (parents, _) in known.values() {
        for parent in parents {
            *children.entry(*parent).or_default() += 1;
        }
    }
    let mut ready: BinaryHeap<(i64, CommitId)> = known
        .iter()
        .filter(|(id, _)| children[*id] == 0)
        .map(|(id, (_, seconds))| (*seconds, *id))
        .collect();
    let mut ordered = Vec::with_capacity(known.len());
    while let Some((_, id)) = ready.pop() {
        let (parents, _) = &known[&id];
        for parent in parents {
            let waiting = children.get_mut(parent).expect("every parent is known");
            *waiting -= 1;
            if *waiting == 0 {
                ready.push((known[parent].1, *parent));
            }
        }
        ordered.push(graph::Commit {
            id,
            parents: parents.clone(),
            truncated: boundary.contains(&id),
        });
    }
    Ok(ordered)
}

fn run(
    paths: &RepoPaths,
    limit: Option<usize>,
    cancel: &AtomicBool,
    tx: &Sender<HistoryEvent>,
) -> Result<(), String> {
    let repo = precondition::open(paths)?;
    let tips = tips(&repo)?;
    let boundary = shallow(paths, &repo);
    let commit_graph = repo.commit_graph().ok();
    let _ = tx.send(HistoryEvent::Started {
        commit_graph: commit_graph.is_some(),
    });
    if tips.is_empty() {
        let _ = tx.send(HistoryEvent::Done { commits: 0 });
        return Ok(());
    }
    if !boundary.is_empty() {
        // The topological walk of gix would look for the parents of a boundary
        // commit, which a shallow repo does not have. A shallow repo is small by
        // construction, so order it here.
        let commits = shallow_order(&repo, &tips, &boundary)?;
        let total = commits.len().min(limit.unwrap_or(usize::MAX));
        for page in commits[..total].chunks(graph::PAGE_SIZE) {
            if cancel.load(Ordering::Relaxed) || tx.send(HistoryEvent::Page(page.to_vec())).is_err()
            {
                return Ok(());
            }
        }
        let _ = tx.send(HistoryEvent::Done { commits: total });
        return Ok(());
    }
    let walk = Builder::from_iters(&repo.objects, tips, None::<Vec<CommitId>>)
        .with_commit_graph(commit_graph)
        .sorting(Sorting::TopoOrder)
        .build()
        .map_err(|e| format!("start the walk: {e}"))?;

    let mut page = Vec::with_capacity(graph::PAGE_SIZE);
    let mut commits = 0usize;
    for info in walk.take(limit.unwrap_or(usize::MAX)) {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let info = info.map_err(|e| format!("walk: {e}"))?;
        page.push(graph::Commit {
            id: info.id,
            parents: info.parent_ids.iter().copied().collect(),
            // Shallow repos never get here (see `shallow_order`).
            truncated: false,
        });
        commits += 1;
        if page.len() == graph::PAGE_SIZE {
            let full = std::mem::replace(&mut page, Vec::with_capacity(graph::PAGE_SIZE));
            if tx.send(HistoryEvent::Page(full)).is_err() {
                return Ok(());
            }
        }
    }
    if !page.is_empty() && tx.send(HistoryEvent::Page(page)).is_err() {
        return Ok(());
    }
    let _ = tx.send(HistoryEvent::Done { commits });
    Ok(())
}

/// What a row shows besides its graph. Read lazily for the rows on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMeta {
    /// The first line of the message.
    pub subject: String,
    pub author: String,
    /// Author time: Unix seconds, and the author's UTC offset in seconds.
    pub seconds: i64,
    pub offset: i32,
}

/// Reads the metadata of `ids`, in order; `None` for a commit that cannot
/// be read (the row stays blank rather than failing the whole batch).
pub fn read_meta(paths: &RepoPaths, ids: &[CommitId]) -> Result<Vec<Option<CommitMeta>>, String> {
    let repo = precondition::open(paths)?;
    Ok(ids
        .iter()
        .map(|id| {
            let commit = repo.find_commit(*id).ok()?;
            let message = commit.message_raw_sloppy();
            let subject = message
                .split(|&b| b == b'\n')
                .next()
                .map(|line| String::from_utf8_lossy(line).trim_end().to_owned())
                .unwrap_or_default();
            let author = commit.author().ok()?;
            let time = author.time().ok()?;
            Some(CommitMeta {
                subject,
                author: String::from_utf8_lossy(author.name).into_owned(),
                seconds: time.seconds,
                offset: time.offset,
            })
        })
        .collect())
}
