//! The pure transition function.

use std::mem;

use crate::event::{ChangeSet, Command, Event, Expect, HeadTarget, ReadScope, RefValue, UndoPoint};
use crate::ids::{OpId, ReadId, RefName, RepoId};
use crate::ops::{GitArgs, OpOutcome, WriteRequest};
use crate::state::{
    Action, DiskState, Freshness, Head, LockState, Notice, OpPhase, QueuedOp, Refusal, RepoState,
    RunningOp, Snapshot,
};
use crate::undo::{self, Reversible, UndoEntry, UndoPending};

/// Opens a repo: the initial state plus the first full read.
pub fn open(id: RepoId) -> (RepoState, Vec<Command>) {
    let mut state = RepoState::new(id);
    let mut commands = Vec::new();
    request_read(&mut state, ReadScope::full(), &mut commands);
    (state, commands)
}

/// `step(state, event) -> (state, commands)`. It does no IO and never
/// panics; events that no longer apply (a stale op id, an old read) are
/// ignored.
pub fn step(mut state: RepoState, event: Event) -> (RepoState, Vec<Command>) {
    let mut commands = Vec::new();
    match event {
        Event::StageRequested(_) => refuse(&mut state, Action::Stage, 11),
        Event::UnstageRequested(_) => refuse(&mut state, Action::Unstage, 11),
        Event::PushRequested { .. } => refuse(&mut state, Action::Push, 17),
        Event::UndoRequested => match state.undo.top() {
            Ok(_) => enqueue(&mut state, WriteRequest::Undo),
            Err(reason) => refuse_because(&mut state, Action::Undo, reason),
        },
        Event::UndoSnapshotTaken { op, tree } => {
            if let UndoPending::Recording { point } = &state.undo.pending
                && point.op == op
            {
                let point = point.clone();
                state.undo.pending = UndoPending::Taken { point, tree };
            }
        }
        Event::CommitRequested { message, amend } => {
            enqueue(&mut state, WriteRequest::Commit { message, amend });
        }
        Event::OpCancelRequested(op) => cancel(&mut state, op, &mut commands),
        Event::AuthAnswered(op, answer) => {
            if let Some(running) = running_mut(&mut state, op)
                && matches!(running.phase, OpPhase::AwaitingAuth(_))
            {
                running.phase = OpPhase::Running { progress: None };
                commands.push(Command::AnswerAuth(op, answer));
            }
        }
        Event::RefreshRequested => request_read(&mut state, ReadScope::full(), &mut commands),
        Event::NoticeDismissed => state.notice = None,
        // Law 11: a change on disk is never patched in; it is read again.
        Event::DiskChanged(ChangeSet(areas)) => {
            if !areas.is_empty() {
                request_read(&mut state, ReadScope(areas), &mut commands);
            }
        }
        Event::StateRead { read, disk } => apply_read(&mut state, read, disk, &mut commands),
        Event::ReadFailed { read, failure } => {
            if let Freshness::Pending { latest } = state.reads.freshness
                && latest == read
            {
                state.reads.freshness = Freshness::Failed { read };
                state.notice = Some(Notice::ReadFailed { read, failure });
            }
        }
        Event::OpProgress(op, progress) => {
            if let Some(running) = running_mut(&mut state, op)
                && let OpPhase::Running { progress: current } = &mut running.phase
            {
                *current = Some(progress);
            }
        }
        Event::AuthNeeded(op, prompt) => {
            if let Some(running) = running_mut(&mut state, op)
                && !matches!(running.phase, OpPhase::Cancelling)
            {
                running.phase = OpPhase::AwaitingAuth(prompt);
            }
        }
        Event::OpFinished(op, outcome) => {
            if let Some(running) = state.ops.running.take_if(|running| running.id == op) {
                finish_undo(
                    &mut state,
                    op,
                    &running.request,
                    outcome == OpOutcome::Succeeded,
                    &mut commands,
                );
                state.notice = Some(Notice::OpEnded { op, outcome });
                // Law 13: whatever happened, the next write starts from disk.
                request_read(&mut state, ReadScope::full(), &mut commands);
            }
        }
    }
    dispatch(&mut state, &mut commands);
    (state, commands)
}

fn refuse(state: &mut RepoState, action: Action, task: u8) {
    refuse_because(state, action, Refusal::NotYetSupported { task });
}

fn refuse_because(state: &mut RepoState, action: Action, reason: Refusal) {
    state.notice = Some(Notice::Refused { action, reason });
}

fn enqueue(state: &mut RepoState, request: WriteRequest) {
    let id = state.ops.next;
    state.ops.next = OpId(id.0.saturating_add(1));
    state.ops.queued.push_back(QueuedOp { id, request });
}

fn running_mut(state: &mut RepoState, op: OpId) -> Option<&mut RunningOp> {
    state
        .ops
        .running
        .as_mut()
        .filter(|running| running.id == op)
}

fn cancel(state: &mut RepoState, op: OpId, commands: &mut Vec<Command>) {
    if let Some(running) = running_mut(state, op) {
        if !matches!(running.phase, OpPhase::Cancelling) {
            running.phase = OpPhase::Cancelling;
            commands.push(Command::CancelOp(op));
        }
    } else if let Some(position) = state.ops.queued.iter().position(|queued| queued.id == op) {
        state.ops.queued.remove(position);
        state.notice = Some(Notice::OpEnded {
            op,
            outcome: OpOutcome::Cancelled,
        });
    }
}

fn request_read(state: &mut RepoState, scope: ReadScope, commands: &mut Vec<Command>) {
    let read = state.reads.next;
    state.reads.next = ReadId(read.0.saturating_add(1));
    state.reads.freshness = Freshness::Pending { latest: read };
    commands.push(Command::ReadState { read, scope });
}

/// Reads are numbered in request order, so a read that lands after a newer
/// one has already been applied is older than the snapshot: drop it.
fn apply_read(state: &mut RepoState, read: ReadId, disk: DiskState, commands: &mut Vec<Command>) {
    if read >= state.reads.next {
        return; // never requested
    }
    if let Snapshot::Read { read: applied, .. } = state.snapshot
        && read <= applied
    {
        return;
    }
    state.snapshot = Snapshot::Read { disk, read };
    let latest = match state.reads.freshness {
        Freshness::Fresh => return,
        Freshness::Pending { latest } => latest,
        Freshness::Failed { read } => read,
    };
    if read >= latest {
        state.reads.freshness = Freshness::Fresh;
        settle_undo(state, commands);
    }
}

/// An operation or an undo finished and the read that follows it has landed:
/// now the stack can learn what the operation left behind.
fn settle_undo(state: &mut RepoState, commands: &mut Vec<Command>) {
    let Some(disk) = state.snapshot.disk().cloned() else {
        return;
    };
    match mem::take(&mut state.undo.pending) {
        UndoPending::Settling { point, tree } => {
            let after_refs = point
                .refs
                .iter()
                .map(|before| RefValue {
                    name: before.name.clone(),
                    oid: undo::current_value(&disk, &before.name),
                })
                .collect();
            let pruned = state.undo.push(UndoEntry {
                point,
                tree,
                after_refs,
                after_index: disk.index,
                reversible: Reversible::Yes,
            });
            prune(pruned, commands);
        }
        UndoPending::Rewinding => {
            if let Some(top) = state.undo.entries.back_mut() {
                top.after_index = disk.index;
            }
        }
        other => state.undo.pending = other,
    }
}

fn prune(ops: Vec<OpId>, commands: &mut Vec<Command>) {
    if !ops.is_empty() {
        commands.push(Command::PruneUndo(ops));
    }
}

/// The running operation ended: decide what its undo point becomes.
fn finish_undo(
    state: &mut RepoState,
    op: OpId,
    request: &WriteRequest,
    succeeded: bool,
    commands: &mut Vec<Command>,
) {
    let pending = mem::take(&mut state.undo.pending);
    state.undo.pending = match (pending, request, succeeded) {
        (UndoPending::Taken { point, tree }, WriteRequest::Commit { .. }, true) => {
            UndoPending::Settling { point, tree }
        }
        (UndoPending::Replaying { entry }, WriteRequest::Undo, true) => {
            prune(vec![entry.point.op], commands);
            UndoPending::Rewinding
        }
        (UndoPending::Replaying { entry }, WriteRequest::Undo, false) => {
            state.undo.entries.push_back(entry);
            UndoPending::Idle
        }
        // Failed or cancelled, or the shell never reported the snapshot:
        // there is nothing to undo, and whatever was saved is dropped.
        (UndoPending::Recording { .. } | UndoPending::Taken { .. }, _, _) => {
            prune(vec![op], commands);
            UndoPending::Idle
        }
        (other, _, _) => other,
    };
}

/// Starts the next queued write when nothing else is writing, the snapshot
/// is fresh and nobody else holds `index.lock` (law 12).
///
/// An undo is planned here, against the snapshot the write will run on, so
/// it is refused when anything moved since the operation it reverses.
fn dispatch(state: &mut RepoState, commands: &mut Vec<Command>) {
    // A refused undo ends right here, so the next queued write may start.
    while state.ops.running.is_none() && !state.ops.queued.is_empty() {
        let Some(disk) = state.fresh_disk() else {
            return;
        };
        if disk.lock != LockState::Free {
            return;
        }
        let expect = Expect {
            head: disk.head.clone(),
            index: disk.index.clone(),
        };
        let disk = disk.clone();
        let Some(QueuedOp { id, request }) = state.ops.queued.pop_front() else {
            return;
        };
        match &request {
            WriteRequest::Commit { message, amend } => {
                let point = UndoPoint {
                    op: id,
                    label: commit_label(message, *amend),
                    head: HeadTarget::of(&disk.head),
                    refs: vec![head_ref(&disk.head)],
                    index: expect.index.clone(),
                };
                commands.push(Command::SnapshotForUndo(point.clone()));
                commands.push(Command::RunGit {
                    op: id,
                    args: GitArgs::Commit {
                        message: message.clone(),
                        amend: *amend,
                    },
                    expect,
                });
                state.undo.pending = UndoPending::Recording { point };
            }
            WriteRequest::Undo => match undo::plan(&state.undo, &disk, id) {
                Ok(replay) => {
                    if let Some(entry) = state.undo.entries.pop_back() {
                        state.undo.pending = UndoPending::Replaying { entry };
                    }
                    commands.push(Command::ReplayUndo(replay));
                }
                Err(reason) => {
                    refuse_because(state, Action::Undo, reason);
                    continue;
                }
            },
        }
        state.ops.running = Some(RunningOp {
            id,
            request,
            phase: OpPhase::Running { progress: None },
        });
    }
}

/// The ref a commit moves: the current branch, or `HEAD` itself when
/// detached.
fn head_ref(head: &Head) -> RefValue {
    match head {
        Head::Branch { name, oid } => RefValue {
            name: name.to_ref(),
            oid: Some(oid.clone()),
        },
        Head::Detached(oid) => RefValue {
            name: RefName("HEAD".into()),
            oid: Some(oid.clone()),
        },
        Head::Unborn(name) => RefValue {
            name: name.to_ref(),
            oid: None,
        },
    }
}

fn commit_label(message: &str, amend: bool) -> String {
    let subject = message.lines().next().unwrap_or("").trim();
    let verb = if amend { "Amend" } else { "Commit" };
    format!("{verb} '{subject}'")
}
