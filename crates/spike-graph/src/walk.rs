//! Background commit walk: gix topological order from HEAD, using the
//! commit-graph file, laid out and sent to the UI in pages of 5,000.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use gix::ObjectId;
use gix::traverse::commit::topo::{Builder, Sorting};

use crate::layout::{Edge, Lanes, Layout, Row};

pub const PAGE: usize = 5_000;

pub struct Page {
    pub ids: Vec<ObjectId>,
    pub rows: Vec<Row>,
    /// Edges for `rows`; row ranges already index the UI's full edge list.
    pub edges: Vec<Edge>,
    /// Lane snapshots for every CHECKPOINT-th row of this page, in order.
    pub checkpoints: Vec<Lanes>,
    /// When this page was complete on the walk thread.
    pub ready_at: Instant,
    pub took: Duration,
}

pub enum WalkEvent {
    Started { commit_graph: bool },
    Page(Page),
    Done { commits: usize, took: Duration },
    Failed(String),
}

pub fn spawn(repo: PathBuf, limit: usize) -> Receiver<WalkEvent> {
    let (tx, rx) = channel();
    thread::Builder::new()
        .name("spike-graph walk".into())
        .spawn(move || {
            if let Err(error) = run(&repo, limit, &tx) {
                tx.send(WalkEvent::Failed(error)).ok();
            }
        })
        .expect("spawn the walk thread");
    rx
}

fn run(path: &PathBuf, limit: usize, tx: &Sender<WalkEvent>) -> Result<(), String> {
    let started = Instant::now();
    let repo = gix::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let head = repo
        .head_id()
        .map_err(|e| format!("resolve HEAD: {e}"))?
        .detach();
    let graph = repo.commit_graph().ok();
    tx.send(WalkEvent::Started {
        commit_graph: graph.is_some(),
    })
    .ok();
    let walk = Builder::from_iters(&repo.objects, [head], None::<Vec<ObjectId>>)
        .with_commit_graph(graph)
        .sorting(Sorting::TopoOrder)
        .build()
        .map_err(|e| format!("start the walk: {e}"))?;

    let mut layout = Layout::default();
    let mut ids = Vec::with_capacity(PAGE);
    let mut edges_sent = 0u32;
    let mut page_start = Instant::now();
    let mut commits = 0;
    for info in walk.take(limit) {
        let info = info.map_err(|e| format!("walk: {e}"))?;
        layout.place(info.id, &info.parent_ids);
        ids.push(info.id);
        commits += 1;
        if ids.len() == PAGE {
            send_page(tx, &mut ids, &mut layout, &mut edges_sent, page_start)?;
            page_start = Instant::now();
        }
    }
    if !ids.is_empty() {
        send_page(tx, &mut ids, &mut layout, &mut edges_sent, page_start)?;
    }
    tx.send(WalkEvent::Done {
        commits,
        took: started.elapsed(),
    })
    .ok();
    Ok(())
}

fn send_page(
    tx: &Sender<WalkEvent>,
    ids: &mut Vec<ObjectId>,
    layout: &mut Layout<ObjectId>,
    edges_sent: &mut u32,
    page_start: Instant,
) -> Result<(), String> {
    // Hand the finished rows to the UI but keep the open lanes, so the next
    // page continues the layout exactly where this one stopped.
    let mut rows = std::mem::take(&mut layout.rows);
    let edges = std::mem::take(&mut layout.edges);
    for row in &mut rows {
        row.edges = row.edges.start + *edges_sent..row.edges.end + *edges_sent;
    }
    *edges_sent += edges.len() as u32;
    let page = Page {
        ids: std::mem::replace(ids, Vec::with_capacity(PAGE)),
        rows,
        edges,
        checkpoints: std::mem::take(&mut layout.checkpoints),
        ready_at: Instant::now(),
        took: page_start.elapsed(),
    };
    tx.send(WalkEvent::Page(page))
        .map_err(|_| "the window closed".to_string())
}
