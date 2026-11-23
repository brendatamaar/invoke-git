//! One open repo: the core's state, the shell that executes its commands,
//! and the file watcher. The views only read `state` and call [`deliver`].
//!
//! Every outcome comes back as an `Event` on one channel; a task on the UI
//! thread steps the core with each one. No business logic lives here: this
//! file only moves events and commands between the pure core and the IO
//! in `shell`.
//!
//! [`deliver`]: RepoSession::deliver

use std::collections::BTreeSet;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use gpui::{Context, Task};
use invoke_core::{
    Area, ChangeSet, Command, Event, OpId, RepoId, RepoState, UndoEntry, UndoPoint, open, step,
};
use shell::undo::Snapshotted;
use shell::{
    DiscardRunner, Discovered, Git, GitRunner, RepoWatcher, StagingRunner, UndoRunner,
    read::read_event,
};

/// Starts closed and opens once; waiting threads then go on.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: std::sync::Condvar,
}

impl Gate {
    fn open(&self) {
        if let Ok(mut open) = self.open.lock() {
            *open = true;
        }
        self.changed.notify_all();
    }

    fn wait(&self) {
        if let Ok(mut open) = self.open.lock() {
            while !*open {
                match self.changed.wait(open) {
                    Ok(next) => open = next,
                    Err(_) => return,
                }
            }
        }
    }
}

pub struct RepoSession {
    pub discovered: Discovered,
    pub state: RepoState,
    pub blame: shell::blame::BlameReader,
    /// Why the watcher could not start, if it could not: the repo still
    /// works, but only changes made here show up.
    pub watch_error: Option<String>,
    events: Sender<Event>,
    runner: Arc<GitRunner>,
    staging: Arc<StagingRunner>,
    discard: Arc<DiscardRunner>,
    undo: Arc<UndoRunner>,
    /// Operations whose undo snapshot failed: their git run is skipped.
    skip: Arc<Mutex<BTreeSet<OpId>>>,
    /// Opens when the undo points of the last run have been checked against
    /// the repo (and the refs of stale ones pruned): no new snapshot is taken
    /// before, so its ref cannot be mistaken for a stale one.
    ready: Arc<Gate>,
    /// The undo points as last written to the store.
    saved_undo: Vec<UndoEntry>,
    _watcher: Option<RepoWatcher>,
    _pump: Task<()>,
}

impl RepoSession {
    pub fn new(discovered: Discovered, git: Git, cx: &mut Context<Self>) -> Self {
        let paths = discovered.paths.clone();
        let (state, commands) = open(RepoId {
            root_commit: None,
            remote_url: None,
        });
        let (events, inbox) = mpsc::channel();
        let (watcher, watch_error) = match RepoWatcher::start(&discovered, events.clone()) {
            Ok(watcher) => (Some(watcher), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let session = Self {
            discovered,
            state,
            blame: shell::blame::BlameReader::new(git.clone(), paths.clone()),
            watch_error,
            events,
            runner: Arc::new(Self::git_runner(git.clone(), paths.clone())),
            staging: Arc::new(StagingRunner::new(git.clone(), paths.clone())),
            discard: Arc::new(DiscardRunner::new(git.clone(), paths.clone())),
            undo: Arc::new(UndoRunner::new(git, paths)),
            skip: Arc::default(),
            ready: Arc::new(Gate::default()),
            saved_undo: Vec::new(),
            _watcher: watcher,
            _pump: Self::pump(inbox, cx),
        };
        session.exec_all(commands, cx);
        session.restore_undo(cx);
        session
    }

    /// Brings back the undo points kept in the store by the last run: the
    /// ones whose saved trees are still in the repo go to the core, the refs of
    /// the others are removed. Whatever happens, the gate opens.
    fn restore_undo(&self, cx: &mut Context<Self>) {
        let (events, ready, undo) = (self.events.clone(), self.ready.clone(), self.undo.clone());
        let paths = self.discovered.paths.clone();
        let repo = crate::notes_db::repo_key(&paths.workdir);
        let db = crate::notes_db::NotesDb::global();
        cx.background_executor()
            .spawn(async move {
                let kept = db.load_undo(&repo).map(|(entries, _skipped)| entries);
                let restored = kept.and_then(|entries| shell::undo::reconcile(&paths, entries));
                if let Ok(restored) = restored {
                    if !restored.orphans.is_empty() {
                        undo.prune(&restored.orphans);
                    }
                    let _ = events.send(Event::UndoStackRestored(restored.kept));
                }
                ready.open();
            })
            .detach();
    }

    /// Writes the undo points to the store when they changed.
    fn persist_undo(&mut self, cx: &mut Context<Self>) {
        let entries: Vec<UndoEntry> = self.state.undo.entries.iter().cloned().collect();
        if entries == self.saved_undo {
            return;
        }
        self.saved_undo = entries.clone();
        static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let repo = crate::notes_db::repo_key(&self.discovered.paths.workdir);
        let db = crate::notes_db::NotesDb::global();
        cx.background_executor()
            .spawn(async move {
                // Best effort: undo that does not survive a restart is the
                // old behaviour, not a failure.
                let _ = db.save_undo(&repo, generation, entries);
            })
            .detach();
    }

    /// The runner for git commands. Network operations ask for credentials in
    /// the app through the helper next to the executable; without it they
    /// fail with git's own message instead of waiting for a console.
    fn git_runner(git: Git, paths: shell::RepoPaths) -> GitRunner {
        let runner = GitRunner::new(git, paths);
        let helper = crate::clone_view::helper_path();
        if helper.is_file() {
            runner.with_askpass(helper)
        } else {
            runner
        }
    }

    /// Steps the core with one event and runs the commands it asks for.
    pub fn deliver(&mut self, event: Event, cx: &mut Context<Self>) {
        let placeholder = RepoState::new(self.state.id.clone());
        let (state, commands) = step(std::mem::replace(&mut self.state, placeholder), event);
        self.state = state;
        self.exec_all(commands, cx);
        self.persist_undo(cx);
        cx.notify();
    }

    /// Waits for events on a background thread and steps the core with each
    /// batch on the UI thread. It ends when every sender is gone, which is
    /// when the session is dropped.
    fn pump(inbox: Receiver<Event>, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let mut inbox = inbox;
            loop {
                let (back, batch) = cx
                    .background_executor()
                    .spawn(async move {
                        let mut batch = Vec::new();
                        if let Ok(first) = inbox.recv() {
                            batch.push(first);
                            while let Ok(next) = inbox.try_recv() {
                                batch.push(next);
                            }
                        }
                        (inbox, batch)
                    })
                    .await;
                inbox = back;
                if batch.is_empty() {
                    break;
                }
                let alive = this.update(cx, |this, cx| {
                    for event in batch {
                        this.deliver(event, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
    }

    /// Runs the commands one step produced. A write that has an undo
    /// snapshot waits for it: the snapshot's event must reach the core
    /// before the write's own `OpFinished`, or the undo point is dropped
    /// (a quick write such as deleting a branch outruns the snapshot).
    fn exec_all(&self, commands: Vec<Command>, cx: &mut Context<Self>) {
        let mut gates: Vec<(OpId, Receiver<()>)> = Vec::new();
        let mut doors: Vec<(OpId, Sender<()>)> = Vec::new();
        for command in &commands {
            if let Command::SnapshotForUndo(point) = command {
                let (done, gate) = mpsc::channel();
                doors.push((point.op, done));
                gates.push((point.op, gate));
            }
        }
        for command in commands {
            match command {
                Command::SnapshotForUndo(point) => {
                    let done = doors
                        .iter()
                        .position(|(op, _)| *op == point.op)
                        .map(|at| doors.remove(at).1);
                    self.snapshot(point, done, cx);
                }
                Command::RunGit { op, .. }
                | Command::ApplyPatchToIndex { op, .. }
                | Command::ApplyPatchToWorktree { op, .. } => {
                    let gate = gates
                        .iter()
                        .position(|(gated, _)| *gated == op)
                        .map(|at| gates.remove(at).1);
                    self.exec(command, gate, cx);
                }
                other => self.exec(other, None, cx),
            }
        }
    }

    fn snapshot(&self, point: UndoPoint, done: Option<Sender<()>>, cx: &mut Context<Self>) {
        let events = self.events.clone();
        let (undo, skip) = (self.undo.clone(), self.skip.clone());
        let ready = self.ready.clone();
        cx.background_executor()
            .spawn(async move {
                ready.wait();
                let event = match undo.snapshot(&point) {
                    Snapshotted::Taken(event) => event,
                    Snapshotted::Failed(event) => {
                        if let Ok(mut skip) = skip.lock() {
                            skip.insert(point.op);
                        }
                        event
                    }
                };
                let _ = events.send(event);
                if let Some(done) = done {
                    let _ = done.send(());
                }
            })
            .detach();
    }

    /// Runs one command. Reads and writes run off the UI thread; each
    /// answers with an event on the channel. A `gate` holds a write back
    /// until its undo snapshot is done.
    fn exec(&self, command: Command, gate: Option<Receiver<()>>, cx: &mut Context<Self>) {
        let events = self.events.clone();
        match command {
            Command::ReadState { read, .. } => {
                let paths = self.discovered.paths.clone();
                cx.background_executor()
                    .spawn(async move {
                        let _ = events.send(read_event(&paths, read));
                    })
                    .detach();
            }
            Command::SnapshotForUndo(_) => {}
            Command::RunGit { op, args, expect } => {
                let (runner, skip) = (self.runner.clone(), self.skip.clone());
                cx.background_executor()
                    .spawn(async move {
                        if let Some(gate) = gate {
                            let _ = gate.recv();
                        }
                        let skipped = skip.lock().is_ok_and(|mut skip| skip.remove(&op));
                        if !skipped {
                            runner.run(op, args, expect, events);
                        }
                    })
                    .detach();
            }
            Command::ApplyPatchToIndex {
                op,
                patch,
                expect_index,
            } => {
                let staging = self.staging.clone();
                cx.background_executor()
                    .spawn(async move {
                        if let Some(gate) = gate {
                            let _ = gate.recv();
                        }
                        let _ = events.send(staging.apply(op, &patch, &expect_index));
                    })
                    .detach();
            }
            Command::ApplyPatchToWorktree {
                op,
                patch,
                path,
                expect_index,
                expect_file,
            } => {
                let skip = self.skip.clone();
                let discard = self.discard.clone();
                cx.background_executor()
                    .spawn(async move {
                        if let Some(gate) = gate {
                            let _ = gate.recv();
                        }
                        if skip.lock().is_ok_and(|mut skip| skip.remove(&op)) {
                            return;
                        }
                        let done =
                            discard.apply(op, &patch, &path, &expect_index, expect_file.as_ref());
                        let _ = events.send(done);
                    })
                    .detach();
            }
            Command::ReplayUndo(replay) => {
                let undo = self.undo.clone();
                cx.background_executor()
                    .spawn(async move {
                        let _ = events.send(undo.replay(&replay));
                    })
                    .detach();
            }
            Command::PruneUndo(ops) => {
                let undo = self.undo.clone();
                cx.background_executor()
                    .spawn(async move { undo.prune(&ops) })
                    .detach();
            }
            Command::RemoveStaleLock => {
                let paths = self.discovered.paths.clone();
                cx.background_executor()
                    .spawn(async move {
                        let _ = shell::remove_index_lock(&paths);
                        // Whether or not it worked, read again: the watcher may
                        // be off, and the lock state is what the user is waiting on.
                        let _ = events.send(Event::DiskChanged(ChangeSet(
                            [Area::Index].into_iter().collect(),
                        )));
                    })
                    .detach();
            }
            Command::CancelOp(op) => self.runner.cancel(op),
            Command::AnswerAuth(op, answer) => self.runner.answer(op, answer),
        }
    }
}
