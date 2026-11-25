//! The pure transition function.

use std::mem;

use crate::advanced::{self, WorktreeSpec, WorktreeTarget};
use crate::branch;
use crate::commit::{self, CommitKind};
use crate::conflict::{self, ConflictRefusal, Operation};
use crate::event::{
    ChangeSet, Command, Event, Expect, HeadTarget, IndexEffect, ReadScope, RefValue, UndoPoint,
};
use crate::ids::{BranchName, Oid, OpId, ReadId, RefName, RepoId, TagName};
use crate::message::CommitMessage;
use crate::ops::FileAction;
use crate::ops::{
    ErrorClass, Failure, GitArgs, Lease, OpOutcome, PullMode, RemoteBranch, StageRequest,
    WriteRequest,
};
use crate::rebase::{
    RebaseControl, RebaseFinish, RebaseIntent, RebaseOrigin, RebaseRun, RebaseState, RebaseUndo,
};
use crate::reflog;
use crate::stash;
use crate::state::{
    Action, BranchRefusal, CommitRefusal, DiskState, Freshness, Head, LockState, Notice, OpPhase,
    Phase, QueuedOp, Refusal, RepoState, RunningOp, Snapshot, TagRefusal,
};
use crate::tag;
use crate::undo::{self, Irreversible, Reversal, Reversible, UndoEntry, UndoPending};

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
        Event::StageRequested(request) => {
            enqueue_index_write(&mut state, Action::Stage, request, WriteRequest::Stage);
        }
        Event::UnstageRequested(request) => {
            enqueue_index_write(&mut state, Action::Unstage, request, WriteRequest::Unstage);
        }
        Event::FileRequested(request) => enqueue(&mut state, WriteRequest::File(request)),
        // Task 13: an empty patch changes nothing and is refused up front.
        Event::DiscardRequested(request) => {
            if request.patch.0.is_empty() {
                refuse_because(&mut state, Action::Discard, Refusal::NothingSelected);
            } else {
                enqueue(&mut state, WriteRequest::Discard(request));
            }
        }
        // Task 17.
        Event::PushRequested {
            to,
            force_with_lease,
            set_upstream,
        } => request_push(&mut state, to, force_with_lease, set_upstream),
        Event::FetchRequested { remote } => enqueue(&mut state, WriteRequest::Fetch { remote }),
        Event::PullRequested { from, mode } => request_pull(&mut state, from, mode),
        Event::SetUpstreamRequested { upstream } => request_set_upstream(&mut state, upstream),
        // Task 23: the conflict flow.
        Event::ResolveFileRequested { path, choice } => {
            request_conflict(&mut state, Action::ResolveFile, |_| {
                Ok(WriteRequest::ResolveFile { path, choice })
            })
        }
        Event::ContinueRequested => request_conflict(&mut state, Action::Continue, |operation| {
            Ok(WriteRequest::Continue { operation })
        }),
        // P2: a single-commit pick or revert onto the `HEAD` the user sees.
        Event::CherryPickRequested(commit) => {
            request_replay(&mut state, Action::CherryPick, commit, |commit, onto| {
                WriteRequest::CherryPick { commit, onto }
            })
        }
        Event::RevertRequested(commit) => {
            request_replay(&mut state, Action::Revert, commit, |commit, onto| {
                WriteRequest::Revert { commit, onto }
            })
        }
        Event::RebasePlanEdited(plan) => match &mut state.rebase {
            RebaseState::Editing { plan: draft, .. } => *draft = plan,
            RebaseState::Idle | RebaseState::Done | RebaseState::Aborted => {
                match state.snapshot.disk() {
                    Some(disk) if disk.phase == Phase::Clean => {
                        state.rebase = RebaseState::Editing {
                            plan,
                            head: disk.head.clone(),
                        };
                    }
                    Some(_) => {
                        refuse_because(&mut state, Action::InteractiveRebase, Refusal::RepoBusy)
                    }
                    None => refuse_because(&mut state, Action::InteractiveRebase, Refusal::NotRead),
                }
            }
            _ => refuse_because(&mut state, Action::InteractiveRebase, Refusal::RepoBusy),
        },
        Event::RebaseStartRequested => {
            if let RebaseState::Editing { plan, head } = &state.rebase {
                let request = WriteRequest::InteractiveRebase {
                    plan: plan.clone(),
                    head: head.clone(),
                };
                state.rebase = RebaseState::Queued(state.ops.next);
                enqueue(&mut state, request);
            } else {
                refuse_because(
                    &mut state,
                    Action::InteractiveRebase,
                    Refusal::NothingSelected,
                );
            }
        }
        // P7: reset the `HEAD` the user sees to a reflog/commit OID.
        Event::ResetRequested { target, mode } => {
            request_replay(&mut state, Action::Reset, target, |target, onto| {
                WriteRequest::Reset { target, mode, onto }
            })
        }
        Event::SkipRequested => request_conflict(&mut state, Action::Skip, |operation| {
            if operation.can_skip() {
                Ok(WriteRequest::Skip { operation })
            } else {
                Err(ConflictRefusal::SkipNotAvailable)
            }
        }),
        Event::AbortRequested => match state.rebase {
            RebaseState::Editing { .. } => state.rebase = RebaseState::Aborted,
            RebaseState::Queued(op) => cancel(&mut state, op, &mut commands),
            RebaseState::Running { ref mut intent, .. }
            | RebaseState::Reading { ref mut intent, .. } => *intent = RebaseIntent::AbortAtPause,
            _ => request_conflict(&mut state, Action::Abort, |operation| {
                Ok(WriteRequest::Abort { operation })
            }),
        },
        Event::UndoRequested => match state.undo.top() {
            Ok(_) => enqueue(&mut state, WriteRequest::Undo),
            Err(reason) => refuse_because(&mut state, Action::Undo, reason),
        },
        // Task 20: stash requests are checked against the fresh snapshot when
        // they are dispatched, so a stash that moved meanwhile is refused.
        Event::StashSaveRequested {
            message,
            include_untracked,
        } => enqueue(
            &mut state,
            WriteRequest::StashSave {
                message,
                include_untracked,
            },
        ),
        Event::StashApplyRequested(stash) => {
            enqueue(&mut state, WriteRequest::StashApply { stash });
        }
        Event::StashPopRequested(stash) => enqueue(&mut state, WriteRequest::StashPop { stash }),
        Event::StashDropRequested(stash) => {
            enqueue(&mut state, WriteRequest::StashDrop { stash });
        }
        // P10: worktrees, submodules, and Git LFS.
        Event::WorktreeAddRequested { path, spec } => {
            match advanced::validate_worktree_path(&path) {
                Err(why) => {
                    refuse_because(&mut state, Action::WorktreeAdd, Refusal::Worktree(why));
                }
                Ok(path) => match spec {
                    WorktreeSpec::ExistingBranch(name) => enqueue(
                        &mut state,
                        WriteRequest::WorktreeAdd {
                            path,
                            target: WorktreeTarget::ExistingBranch(name),
                        },
                    ),
                    WorktreeSpec::NewBranch { name, at } => match BranchName::parse(&name) {
                        Ok(name) => enqueue(
                            &mut state,
                            WriteRequest::WorktreeAdd {
                                path,
                                target: WorktreeTarget::NewBranch { name, at },
                            },
                        ),
                        Err(problem) => refuse_because(
                            &mut state,
                            Action::WorktreeAdd,
                            Refusal::Worktree(advanced::WorktreeRefusal::InvalidBranch(problem)),
                        ),
                    },
                    WorktreeSpec::Detach(at) => enqueue(
                        &mut state,
                        WriteRequest::WorktreeAdd {
                            path,
                            target: WorktreeTarget::Detach(at),
                        },
                    ),
                },
            }
        }
        Event::WorktreeRemoveRequested { path, force } => {
            match advanced::validate_worktree_path(&path) {
                Ok(path) => enqueue(&mut state, WriteRequest::WorktreeRemove { path, force }),
                Err(why) => {
                    refuse_because(&mut state, Action::WorktreeRemove, Refusal::Worktree(why))
                }
            }
        }
        Event::SubmoduleRequested { op, path } => {
            enqueue(&mut state, WriteRequest::Submodule { op, path });
        }
        Event::LfsPullRequested { path } => {
            enqueue(&mut state, WriteRequest::LfsPull { path });
        }
        // P9: pull/merge request checkout.
        Event::CheckoutPrRequested {
            remote,
            remote_ref,
            local_branch,
        } => {
            let remote = remote.trim().to_owned();
            if remote.is_empty()
                || remote.starts_with('-')
                || remote.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                refuse_because(
                    &mut state,
                    Action::CheckoutPr,
                    Refusal::PrCheckout(crate::PrCheckoutRefusal::InvalidRemote),
                );
            } else if !crate::forge::is_valid_pr_ref(&remote_ref) {
                refuse_because(
                    &mut state,
                    Action::CheckoutPr,
                    Refusal::PrCheckout(crate::PrCheckoutRefusal::InvalidRef),
                );
            } else {
                match BranchName::parse(&local_branch) {
                    Ok(local_branch) => enqueue(
                        &mut state,
                        WriteRequest::CheckoutPr {
                            remote,
                            remote_ref,
                            local_branch,
                        },
                    ),
                    Err(problem) => refuse_because(
                        &mut state,
                        Action::CheckoutPr,
                        Refusal::PrCheckout(crate::PrCheckoutRefusal::InvalidBranch(problem)),
                    ),
                }
            }
        }
        Event::UndoSnapshotTaken { op, tree } => {
            if let UndoPending::Recording { point } = &state.undo.pending
                && point.op == op
            {
                let point = point.clone();
                state.undo.pending = UndoPending::Taken { point, tree };
            }
        }
        // Task 14: the message is cleaned and checked here, at the boundary;
        // what depends on the disk is checked when the request is dispatched.
        Event::CommitRequested { message, amend } => match CommitMessage::parse(&message) {
            Ok(message) => enqueue(&mut state, WriteRequest::Commit { message, amend }),
            Err(problem) => refuse_because(
                &mut state,
                Action::Commit,
                Refusal::Commit(CommitRefusal::Message(problem)),
            ),
        },
        // Task 15: branches. Names are parsed here; everything that depends
        // on the disk is checked when the request is dispatched.
        Event::BranchCreateRequested { name, at, switch } => match BranchName::parse(&name) {
            Ok(name) => enqueue(&mut state, WriteRequest::CreateBranch { name, at, switch }),
            Err(problem) => refuse_because(
                &mut state,
                Action::CreateBranch,
                Refusal::Branch(BranchRefusal::InvalidName(problem)),
            ),
        },
        Event::BranchSwitchRequested(name) => {
            enqueue(&mut state, WriteRequest::SwitchBranch { name });
        }
        Event::BranchRenameRequested { from, to } => match BranchName::parse(&to) {
            Ok(to) => enqueue(&mut state, WriteRequest::RenameBranch { from, to }),
            Err(problem) => refuse_because(
                &mut state,
                Action::RenameBranch,
                Refusal::Branch(BranchRefusal::InvalidName(problem)),
            ),
        },
        Event::BranchDeleteRequested { name, force } => {
            enqueue(&mut state, WriteRequest::DeleteBranch { name, force });
        }
        // P3: tags. Names and messages are parsed here; what depends on the
        // disk is checked when the request is dispatched.
        Event::TagCreateRequested { name, at, message } => {
            match (
                TagName::parse(&name),
                message.as_deref().map(CommitMessage::parse),
            ) {
                (Err(problem), _) => refuse_because(
                    &mut state,
                    Action::CreateTag,
                    Refusal::Tag(TagRefusal::InvalidName(problem)),
                ),
                (_, Some(Err(problem))) => refuse_because(
                    &mut state,
                    Action::CreateTag,
                    Refusal::Tag(TagRefusal::Message(problem)),
                ),
                (Ok(name), message) => {
                    let message = message.and_then(Result::ok);
                    enqueue(&mut state, WriteRequest::CreateTag { name, at, message });
                }
            }
        }
        Event::TagDeleteRequested(name) => enqueue(&mut state, WriteRequest::DeleteTag { name }),
        Event::TagPushRequested { remote, name } => {
            enqueue(&mut state, WriteRequest::PushTag { remote, name });
        }
        Event::RemoteTagDeleteRequested { remote, name } => {
            enqueue(&mut state, WriteRequest::DeleteRemoteTag { remote, name });
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
        Event::RemoveStaleLockRequested => remove_stale_lock(&mut state, &mut commands),
        Event::UndoStackRestored(entries) => {
            if let Some(restored) = state.undo.restore(entries) {
                // Op ids name the saved trees of the restored points, so new
                // operations must not reuse them.
                if let Some(max) = restored.max_op {
                    state.ops.next = OpId(state.ops.next.0.max(max.0 + 1));
                }
                if !restored.dropped.is_empty() {
                    commands.push(Command::PruneUndo(restored.dropped));
                }
            }
        }
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
                let succeeded = outcome == OpOutcome::Succeeded;
                if matches!(state.rebase, RebaseState::Running { .. }) {
                    finish_rebase(&mut state, &running.request, succeeded, &mut commands);
                } else {
                    finish_undo(&mut state, op, &running.request, succeeded, &mut commands);
                }
                state.notice = Some(match (&running.request, outcome) {
                    // A rejected push is a state change, not an error.
                    (
                        WriteRequest::Push { .. },
                        OpOutcome::Failed(
                            failure @ Failure {
                                class: ErrorClass::Expected,
                                ..
                            },
                        ),
                    ) => Notice::PushRejected { op, failure },
                    (_, outcome) => Notice::OpEnded { op, outcome },
                });
                // Law 13: whatever happened, the next write starts from disk.
                request_read(&mut state, ReadScope::full(), &mut commands);
            }
        }
    }
    dispatch(&mut state, &mut commands);
    (state, commands)
}

fn refuse_because(state: &mut RepoState, action: Action, reason: Refusal) {
    state.notice = Some(Notice::Refused { action, reason });
}

/// Task 17. A lock another process holds may be a leftover (a git we killed
/// on cancel, a crash). It is removed only on the user's say-so, and only
/// when the snapshot shows one and none of our own operations is running or
/// waiting, since then the lock cannot be ours.
fn remove_stale_lock(state: &mut RepoState, commands: &mut Vec<Command>) {
    let held = matches!(
        &state.snapshot,
        Snapshot::Read { disk, .. } if disk.lock == LockState::HeldByOther
    );
    if held && state.ops.running.is_none() && state.ops.queued.is_empty() {
        state.notice = None;
        commands.push(Command::RemoveStaleLock);
    } else {
        refuse_because(state, Action::RemoveLock, Refusal::NoStaleLock);
    }
}

/// Task 11: an empty patch changes nothing and is refused up front; the
/// rest waits its turn like any write.
fn enqueue_index_write(
    state: &mut RepoState,
    action: Action,
    request: StageRequest,
    wrap: fn(StageRequest) -> WriteRequest,
) {
    if request.patch.0.is_empty() {
        refuse_because(state, action, Refusal::NothingSelected);
    } else {
        enqueue(state, wrap(request));
    }
}

fn enqueue(state: &mut RepoState, request: WriteRequest) {
    let id = state.ops.next;
    state.ops.next = OpId(id.0.saturating_add(1));
    state.ops.queued.push_back(QueuedOp { id, request });
    // An earlier refusal is stale once a request is accepted.
    if matches!(state.notice, Some(Notice::Refused { .. })) {
        state.notice = None;
    }
}

fn enqueue_front(state: &mut RepoState, request: WriteRequest) {
    let id = state.ops.next;
    state.ops.next = OpId(id.0.saturating_add(1));
    state.ops.queued.push_front(QueuedOp { id, request });
    if matches!(state.notice, Some(Notice::Refused { .. })) {
        state.notice = None;
    }
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
        // Task 20: `git stash` is not atomic (it saves the stash, then
        // resets the worktree), so a running stash op is never killed:
        // law 13 would not hold. It is local and quick.
        // Task 23: the same for the conflict flow. Killing an abort or a
        // continue half way would leave git between two states of its own.
        // Task 13: a discard is one patch run plus the records around it;
        // killing it between the two would lose the undo.
        // P7: `git reset` is local and quick; killing `--mixed` mid-write could
        // leave the ref updated without its index reset (law 13).
        if is_stash(&running.request)
            || is_conflict_flow(&running.request)
            || matches!(
                running.request,
                WriteRequest::Discard(_) | WriteRequest::File(_) | WriteRequest::Reset { .. }
            )
        {
            return;
        }
        if !matches!(running.phase, OpPhase::Cancelling) {
            running.phase = OpPhase::Cancelling;
            commands.push(Command::CancelOp(op));
        }
    } else if let Some(position) = state.ops.queued.iter().position(|queued| queued.id == op) {
        state.ops.queued.remove(position);
        if state.rebase == RebaseState::Queued(op) {
            state.rebase = RebaseState::Aborted;
        }
        state.notice = Some(Notice::OpEnded {
            op,
            outcome: OpOutcome::Cancelled,
        });
    }
}

fn is_stash(request: &WriteRequest) -> bool {
    matches!(
        request,
        WriteRequest::StashSave { .. }
            | WriteRequest::StashApply { .. }
            | WriteRequest::StashPop { .. }
            | WriteRequest::StashDrop { .. }
    )
}

fn is_conflict_flow(request: &WriteRequest) -> bool {
    matches!(
        request,
        WriteRequest::ResolveFile { .. }
            | WriteRequest::Continue { .. }
            | WriteRequest::Skip { .. }
            | WriteRequest::Abort { .. }
            // P2: git stops half way if killed; it is local and quick.
            | WriteRequest::CherryPick { .. }
            | WriteRequest::Revert { .. }
            | WriteRequest::InteractiveRebase { .. }
    )
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
        settle_rebase(state, commands);
        settle_undo(state, commands);
    }
}

fn finish_rebase(
    state: &mut RepoState,
    request: &WriteRequest,
    succeeded: bool,
    commands: &mut Vec<Command>,
) {
    let RebaseState::Running { run, intent } = mem::take(&mut state.rebase) else {
        return;
    };
    let saved = match run {
        RebaseRun::Starting(origin) => match mem::take(&mut state.undo.pending) {
            UndoPending::Taken { point, tree } => Some(Box::new(RebaseUndo {
                point,
                tree,
                origin,
            })),
            UndoPending::Recording { point } => {
                prune(vec![point.op], commands);
                None
            }
            _ => None,
        },
        RebaseRun::Resuming(saved) => Some(saved),
    };
    if let Some(saved) = saved {
        let finish = match (succeeded, request) {
            (true, WriteRequest::Abort { .. }) => RebaseFinish::Aborted,
            (true, _) => RebaseFinish::Completed,
            (false, _) => RebaseFinish::Failed,
        };
        state.rebase = RebaseState::Reading {
            saved,
            finish,
            intent,
        };
    }
}

fn settle_rebase(state: &mut RepoState, commands: &mut Vec<Command>) {
    let Some(disk) = state.snapshot.disk() else {
        return;
    };
    let matches_origin = |saved: &RebaseUndo| matches!(&disk.phase, Phase::Rebasing { origin: Some(origin), .. } if origin == &saved.origin);
    let mut requested_abort = None;
    state.rebase = match mem::take(&mut state.rebase) {
        RebaseState::Reading { saved, intent, .. } if matches_origin(&saved) => {
            if intent == RebaseIntent::AbortAtPause {
                requested_abort = disk.phase.operation();
            }
            RebaseState::Paused(saved)
        }
        RebaseState::Reading {
            saved,
            finish: RebaseFinish::Completed,
            ..
        } if disk.phase == Phase::Clean && HeadTarget::of(&disk.head) == saved.point.head => {
            state.undo.pending = UndoPending::Settling {
                point: saved.point,
                tree: saved.tree,
            };
            RebaseState::Done
        }
        RebaseState::Reading { saved, finish, .. } => {
            prune(vec![saved.point.op], commands);
            if finish == RebaseFinish::Aborted {
                // Git may rewrite the index on abort even though its tree is restored.
                // Like undo, re-anchor only if the last point was current at start.
                if RebaseOrigin::from_head(&disk.head).as_ref() == Some(&saved.origin)
                    && let Some(top) = state.undo.entries.back_mut()
                    && top.after_index == saved.point.index
                    && top.after_head == saved.point.head
                {
                    top.after_index = disk.index.clone();
                }
                RebaseState::Aborted
            } else {
                RebaseState::Idle
            }
        }
        RebaseState::Paused(saved) if !matches_origin(&saved) => {
            prune(vec![saved.point.op], commands);
            RebaseState::Idle
        }
        other => other,
    };
    if let Some(operation) = requested_abort {
        enqueue_front(state, WriteRequest::Abort { operation });
    }
}

fn resume_rebase(state: &mut RepoState, args: GitArgs) -> GitArgs {
    let (control, step, total) = match args {
        GitArgs::Continue {
            operation: Operation::Rebase { step, total },
        } => (RebaseControl::Continue, step, total),
        GitArgs::Skip {
            operation: Operation::Rebase { step, total },
        } => (RebaseControl::Skip, step, total),
        GitArgs::Abort {
            operation: Operation::Rebase { step, total },
        } => (RebaseControl::Abort, step, total),
        _ => return args,
    };
    if !matches!(state.rebase, RebaseState::Paused(_)) {
        return args;
    }
    if let RebaseState::Paused(saved) = mem::take(&mut state.rebase) {
        let origin = saved.origin.clone();
        state.rebase = RebaseState::Running {
            run: RebaseRun::Resuming(saved),
            intent: RebaseIntent::Run,
        };
        GitArgs::GuardedRebase {
            control,
            step,
            total,
            origin,
        }
    } else {
        args
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
            let reversible = match &point.reverse {
                Reversal::Never(why) => Reversible::No(*why),
                Reversal::StashKept(_) => Reversible::No(Irreversible::StashNotOnTop),
                Reversal::Refs | Reversal::StashStore(_) | Reversal::Discard { .. } => {
                    Reversible::Yes
                }
            };
            let pruned = state.undo.push(UndoEntry {
                point,
                tree,
                after_refs,
                after_index: disk.index,
                after_head: HeadTarget::of(&disk.head),
                reversible,
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
        (UndoPending::Taken { point, tree }, request, true) if request.takes_undo_point() => {
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
                let kind = match commit::plan(&disk, *amend) {
                    Ok(kind) => kind,
                    Err(reason) => {
                        refuse_because(state, Action::Commit, reason);
                        continue;
                    }
                };
                // A merge commit is a marker an undo stops at (see
                // `Irreversible::MergeConcluded`); the others restore refs.
                let reverse = match kind {
                    CommitKind::ConcludeMerge => Reversal::Never(Irreversible::MergeConcluded),
                    CommitKind::Commit | CommitKind::Amend => Reversal::Refs,
                };
                // At an edit stop the amend belongs to the rebase's undo.
                if !matches!(disk.phase, Phase::Rebasing { .. }) {
                    let point = UndoPoint {
                        op: id,
                        label: message.label(kind.verb()),
                        head: HeadTarget::of(&disk.head),
                        refs: vec![head_ref(&disk.head)],
                        index: expect.index.clone(),
                        index_effect: IndexEffect::Rewrites,
                        reverse,
                    };
                    commands.push(Command::SnapshotForUndo(point.clone()));
                    state.undo.pending = UndoPending::Recording { point };
                }
                commands.push(Command::RunGit {
                    op: id,
                    args: GitArgs::Commit {
                        message: message.as_str().to_owned(),
                        amend: *amend,
                    },
                    expect,
                });
            }
            // Task 11. The patch was built from a diff of one index; if the
            // index on disk is another, the diff is stale and nothing runs.
            WriteRequest::Stage(stage) | WriteRequest::Unstage(stage) => {
                let (action, verb) = if matches!(request, WriteRequest::Stage(_)) {
                    (Action::Stage, "Stage")
                } else {
                    (Action::Unstage, "Unstage")
                };
                if stage.based_on != expect.index {
                    refuse_because(state, action, Refusal::StaleDiff);
                    continue;
                }
                // Index edits at a pause belong to the rebase's undo.
                if !matches!(state.rebase, RebaseState::Paused(_)) {
                    // Only the index changes, so the point records no refs.
                    let point = UndoPoint {
                        op: id,
                        label: stage.selection.label(verb),
                        head: HeadTarget::of(&disk.head),
                        refs: Vec::new(),
                        index: expect.index.clone(),
                        index_effect: IndexEffect::Rewrites,
                        reverse: Reversal::Refs,
                    };
                    commands.push(Command::SnapshotForUndo(point.clone()));
                    state.undo.pending = UndoPending::Recording { point };
                }
                commands.push(Command::ApplyPatchToIndex {
                    op: id,
                    patch: stage.patch.clone(),
                    expect_index: expect.index.clone(),
                });
            }
            // Task 13. Refused in a merge, rebase, cherry-pick or revert, like
            // the other writes that rewrite files, and when the diff the patch
            // was built from is older than the index on disk. The file's own
            // bytes are checked by the shell (`expect_file`, rule 5).
            WriteRequest::Discard(discard) => {
                if disk.phase != Phase::Clean {
                    refuse_because(state, Action::Discard, Refusal::RepoBusy);
                    continue;
                }
                if discard.based_on != expect.index {
                    refuse_because(state, Action::Discard, Refusal::StaleDiff);
                    continue;
                }
                let path = discard.selection.path().clone();
                // Neither a ref nor the index changes.
                let point = UndoPoint {
                    op: id,
                    label: discard.selection.label("Discard"),
                    head: HeadTarget::of(&disk.head),
                    refs: Vec::new(),
                    index: expect.index.clone(),
                    index_effect: IndexEffect::Untouched,
                    reverse: Reversal::Discard { path: path.clone() },
                };
                commands.push(Command::SnapshotForUndo(point.clone()));
                commands.push(Command::ApplyPatchToWorktree {
                    op: id,
                    patch: discard.patch.clone(),
                    path,
                    expect_index: expect.index.clone(),
                    expect_file: discard.expect_file.clone(),
                });
                state.undo.pending = UndoPending::Recording { point };
            }
            WriteRequest::File(file) => {
                let path = &file.path;
                let (action, verb, args) = match &file.action {
                    FileAction::Stage => (Action::Stage, "Stage", GitArgs::AddPath(path.clone())),
                    FileAction::Unstage => (
                        Action::Unstage,
                        "Unstage",
                        GitArgs::RestoreStagedPath(path.clone()),
                    ),
                    FileAction::Discard { file } => (
                        Action::Discard,
                        "Discard",
                        GitArgs::DiscardPath {
                            path: path.clone(),
                            file: file.clone(),
                        },
                    ),
                };
                if action == Action::Discard && disk.phase != Phase::Clean {
                    refuse_because(state, action, Refusal::RepoBusy);
                    continue;
                }
                if file.based_on != expect.index {
                    refuse_because(state, action, Refusal::StaleDiff);
                    continue;
                }
                let (index_effect, reverse) = if action == Action::Discard {
                    (
                        IndexEffect::Untouched,
                        Reversal::Discard { path: path.clone() },
                    )
                } else {
                    (IndexEffect::RestoreBytes, Reversal::Refs)
                };
                if !matches!(state.rebase, RebaseState::Paused(_)) {
                    let point = UndoPoint {
                        op: id,
                        label: format!("{verb} {}", path.0),
                        head: HeadTarget::of(&disk.head),
                        refs: Vec::new(),
                        index: expect.index.clone(),
                        index_effect,
                        reverse,
                    };
                    commands.push(Command::SnapshotForUndo(point.clone()));
                    state.undo.pending = UndoPending::Recording { point };
                }
                commands.push(Command::RunGit {
                    op: id,
                    args,
                    expect,
                });
            }
            // Task 20: stash. Planned against the snapshot the write runs on.
            WriteRequest::StashSave { .. }
            | WriteRequest::StashApply { .. }
            | WriteRequest::StashPop { .. }
            | WriteRequest::StashDrop { .. } => match plan_stash(&request, &disk, id) {
                Ok((point, args)) => {
                    commands.push(Command::SnapshotForUndo(point.clone()));
                    commands.push(Command::RunGit {
                        op: id,
                        args,
                        expect,
                    });
                    state.undo.pending = UndoPending::Recording { point };
                }
                Err((action, reason)) => {
                    refuse_because(state, action, reason);
                    continue;
                }
            },
            // Task 17. Fetch and set-upstream move nothing an undo restores.
            WriteRequest::Fetch { remote } => {
                // Task 23: network operations are paused while a merge,
                // rebase, cherry-pick or revert is (PRD -> "State machines").
                if disk.phase != Phase::Clean {
                    refuse_because(state, Action::Fetch, Refusal::RepoBusy);
                    continue;
                }
                commands.push(Command::RunGit {
                    op: id,
                    args: GitArgs::Fetch {
                        remote: remote.clone(),
                    },
                    expect,
                });
            }
            WriteRequest::SetUpstream { local, upstream } => commands.push(Command::RunGit {
                op: id,
                args: GitArgs::SetUpstream {
                    local: local.clone(),
                    upstream: upstream.clone(),
                },
                expect,
            }),
            WriteRequest::Pull { local, from, mode } => {
                if let Err(reason) = still_on(&disk, local) {
                    refuse_because(state, Action::Pull, reason);
                    continue;
                }
                if disk.phase != Phase::Clean {
                    refuse_because(state, Action::Pull, Refusal::RepoBusy);
                    continue;
                }
                let label = format!("Pull '{}' from {}/{}", local.0, from.remote, from.branch.0);
                let args = GitArgs::Pull {
                    from: from.clone(),
                    mode: *mode,
                };
                let rewrites = IndexEffect::Rewrites;
                run_with_point(
                    state,
                    commands,
                    (id, label),
                    (&disk, expect),
                    args,
                    (rewrites, Irreversible::Pull),
                );
            }
            WriteRequest::Push {
                local,
                to,
                lease,
                set_upstream,
            } => {
                if let Err(reason) = still_on(&disk, local) {
                    refuse_because(state, Action::Push, reason);
                    continue;
                }
                // Task 23: a paused operation cannot be pushed.
                if disk.phase != Phase::Clean {
                    refuse_because(state, Action::Push, Refusal::RepoBusy);
                    continue;
                }
                let label = format!("Push '{}' to {}/{}", local.0, to.remote, to.branch.0);
                let args = GitArgs::Push {
                    local: local.clone(),
                    to: to.clone(),
                    lease: lease.clone(),
                    set_upstream: *set_upstream,
                };
                let untouched = IndexEffect::Untouched;
                run_with_point(
                    state,
                    commands,
                    (id, label),
                    (&disk, expect),
                    args,
                    (untouched, Irreversible::Push),
                );
            }
            // Task 23. Planned against the snapshot the write runs on: a
            // Continue is refused unless every file is Resolved right now.
            WriteRequest::ResolveFile { path, choice } => {
                match conflict::plan_resolve(&disk, path, *choice) {
                    Ok(args) => commands.push(Command::RunGit {
                        op: id,
                        args,
                        expect,
                    }),
                    Err(reason) => {
                        refuse_conflict(state, Action::ResolveFile, reason);
                        continue;
                    }
                }
            }
            WriteRequest::Continue { operation } => {
                match conflict::plan_continue(&disk, *operation) {
                    Ok(args) => {
                        let args = resume_rebase(state, args);
                        if matches!(args, GitArgs::GuardedRebase { .. }) {
                            commands.push(Command::RunGit {
                                op: id,
                                args,
                                expect,
                            });
                        } else {
                            let label = format!("Continue {}", operation_name(*operation));
                            let rewrites = IndexEffect::Rewrites;
                            run_with_point(
                                state,
                                commands,
                                (id, label),
                                (&disk, expect),
                                args,
                                (rewrites, Irreversible::Continued),
                            );
                        }
                    }
                    Err(reason) => {
                        refuse_conflict(state, Action::Continue, reason);
                        continue;
                    }
                }
            }
            WriteRequest::Skip { operation } => match conflict::plan_skip(&disk, *operation) {
                Ok(args) => {
                    let args = resume_rebase(state, args);
                    commands.push(Command::RunGit {
                        op: id,
                        args,
                        expect,
                    });
                }
                Err(reason) => {
                    refuse_conflict(state, Action::Skip, reason);
                    continue;
                }
            },
            WriteRequest::Abort { operation } => match conflict::plan_abort(&disk, *operation) {
                Ok(args) => {
                    let args = resume_rebase(state, args);
                    commands.push(Command::RunGit {
                        op: id,
                        args,
                        expect,
                    });
                }
                Err(reason) => {
                    refuse_conflict(state, Action::Abort, reason);
                    continue;
                }
            },
            WriteRequest::InteractiveRebase { plan, head } => {
                let result = if disk.phase != Phase::Clean {
                    Err(Refusal::RepoBusy)
                } else if &disk.head != head {
                    Err(Refusal::BranchChanged)
                } else if disk.status.has_tracked_changes() {
                    Err(Refusal::UncommittedChanges)
                } else {
                    RebaseOrigin::from_head(head)
                        .ok_or(Refusal::Branch(BranchRefusal::NoCommitsYet))
                };
                let origin = match result {
                    Ok(origin) => origin,
                    Err(reason) => {
                        state.rebase = if matches!(reason, Refusal::BranchChanged) {
                            RebaseState::Idle
                        } else {
                            RebaseState::Editing {
                                plan: plan.clone(),
                                head: head.clone(),
                            }
                        };
                        refuse_because(state, Action::InteractiveRebase, reason);
                        continue;
                    }
                };
                let point = UndoPoint {
                    op: id,
                    label: "Interactive rebase".to_owned(),
                    head: HeadTarget::of(head),
                    refs: vec![head_ref(head)],
                    index: expect.index.clone(),
                    index_effect: IndexEffect::Rewrites,
                    reverse: Reversal::Refs,
                };
                commands.push(Command::SnapshotForUndo(point.clone()));
                commands.push(Command::RunGit {
                    op: id,
                    args: GitArgs::InteractiveRebase(plan.clone()),
                    expect,
                });
                state.undo.pending = UndoPending::Recording { point };
                state.rebase = RebaseState::Running {
                    run: RebaseRun::Starting(origin),
                    intent: RebaseIntent::Run,
                };
            }
            // P2. Starting needs a clean repo with a commit to build on, and
            // the `HEAD` the user saw (law 9). A clean result is one commit
            // on `HEAD`, undone like a commit (refs and index). A conflict
            // is not a failure of the request: git is paused, the point is
            // dropped, and the conflict flow takes over (abort is the way back).
            WriteRequest::CherryPick { commit, onto } | WriteRequest::Revert { commit, onto } => {
                let (action, verb, args) = if matches!(request, WriteRequest::CherryPick { .. }) {
                    (
                        Action::CherryPick,
                        "Cherry-pick",
                        GitArgs::CherryPick {
                            commit: commit.clone(),
                        },
                    )
                } else {
                    (
                        Action::Revert,
                        "Revert",
                        GitArgs::Revert {
                            commit: commit.clone(),
                        },
                    )
                };
                if disk.phase != Phase::Clean {
                    refuse_because(state, action, Refusal::RepoBusy);
                    continue;
                }
                if matches!(disk.head, Head::Unborn(_)) {
                    refuse_because(state, action, Refusal::Branch(BranchRefusal::NoCommitsYet));
                    continue;
                }
                if disk.status.has_tracked_changes() {
                    refuse_because(state, action, Refusal::UncommittedChanges);
                    continue;
                }
                if HeadTarget::of(&disk.head) != *onto {
                    refuse_because(state, action, Refusal::BranchChanged);
                    continue;
                }
                let short: String = commit.to_string().chars().take(7).collect();
                let point = UndoPoint {
                    op: id,
                    label: format!("{verb} {short}"),
                    head: HeadTarget::of(&disk.head),
                    refs: vec![head_ref(&disk.head)],
                    index: expect.index.clone(),
                    index_effect: IndexEffect::Rewrites,
                    reverse: Reversal::Refs,
                };
                commands.push(Command::SnapshotForUndo(point.clone()));
                commands.push(Command::RunGit {
                    op: id,
                    args,
                    expect,
                });
                state.undo.pending = UndoPending::Recording { point };
            }
            // P7: reset the current ref (branch or detached HEAD) to a target
            // commit (e.g. a reflog entry) with an expected-ref guard and an
            // undo snapshot (laws 8, 9, 12).
            WriteRequest::Reset { target, mode, onto } => {
                match reflog::plan_reset(&disk, id, target, *mode, onto) {
                    Ok((args, point)) => {
                        commands.push(Command::SnapshotForUndo(point.clone()));
                        commands.push(Command::RunGit {
                            op: id,
                            args,
                            expect,
                        });
                        state.undo.pending = UndoPending::Recording { point };
                    }
                    Err(reason) => {
                        refuse_because(state, Action::Reset, reason);
                        continue;
                    }
                }
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
            // P3: tags. A local tag has an undo point that restores its ref;
            // one that reaches a remote only leaves the irreversible marker.
            tag_request if tag::action_of(tag_request).is_some() => {
                let Some(planned) = tag::plan(tag_request, &disk) else {
                    continue;
                };
                match planned {
                    Ok(planned) if tag::is_remote(tag_request) => {
                        // Network operations pause with the repo (as push).
                        run_with_point(
                            state,
                            commands,
                            (id, planned.label),
                            (&disk, expect),
                            planned.args,
                            (IndexEffect::Untouched, Irreversible::Push),
                        );
                    }
                    Ok(planned) => {
                        let point = planned.point(id);
                        commands.push(Command::SnapshotForUndo(point.clone()));
                        commands.push(Command::RunGit {
                            op: id,
                            args: planned.args,
                            expect,
                        });
                        state.undo.pending = UndoPending::Recording { point };
                    }
                    Err(reason) => {
                        if let Some(action) = tag::action_of(tag_request) {
                            refuse_because(state, action, reason);
                        }
                        continue;
                    }
                }
            }
            // P10: worktrees, submodules, and Git LFS.
            adv_request if advanced::action_of(adv_request).is_some() => {
                let Some(planned) = advanced::plan(adv_request, &disk) else {
                    continue;
                };
                match planned {
                    Ok(args) => commands.push(Command::RunGit {
                        op: id,
                        args,
                        expect,
                    }),
                    Err(reason) => {
                        if let Some(action) = advanced::action_of(adv_request) {
                            refuse_because(state, action, reason);
                        }
                        continue;
                    }
                }
            }
            WriteRequest::CheckoutPr {
                remote,
                remote_ref,
                local_branch,
            } => {
                if disk.phase != Phase::Clean {
                    refuse_because(state, Action::CheckoutPr, Refusal::RepoBusy);
                    continue;
                }
                if !disk.status.entries.is_empty() {
                    refuse_because(
                        state,
                        Action::CheckoutPr,
                        Refusal::PrCheckout(crate::PrCheckoutRefusal::DirtyWorktree),
                    );
                    continue;
                }
                if disk.refs.refs.contains_key(&local_branch.to_ref()) {
                    refuse_because(
                        state,
                        Action::CheckoutPr,
                        Refusal::PrCheckout(crate::PrCheckoutRefusal::BranchAlreadyExists(
                            local_branch.clone(),
                        )),
                    );
                    continue;
                }
                let prefix = format!("{}/", local_branch.to_ref().0);
                let target_ref = local_branch.to_ref().0;
                if let Some(conflict) = disk.refs.refs.keys().find_map(|existing| {
                    let short = existing.0.strip_prefix("refs/heads/")?;
                    if existing.0.starts_with(&prefix)
                        || target_ref.starts_with(&format!("{}/", existing.0))
                    {
                        Some(BranchName(short.to_owned()))
                    } else {
                        None
                    }
                }) {
                    refuse_because(
                        state,
                        Action::CheckoutPr,
                        Refusal::PrCheckout(crate::PrCheckoutRefusal::BranchNameConflict(conflict)),
                    );
                    continue;
                }
                commands.push(Command::RunGit {
                    op: id,
                    args: GitArgs::CheckoutPr {
                        remote: remote.clone(),
                        remote_ref: remote_ref.clone(),
                        local_branch: local_branch.clone(),
                    },
                    expect,
                });
            }
            branch_request => match branch::plan(branch_request, &disk, id) {
                Some(Ok((args, point))) => {
                    commands.push(Command::SnapshotForUndo(point.clone()));
                    commands.push(Command::RunGit {
                        op: id,
                        args,
                        expect,
                    });
                    state.undo.pending = UndoPending::Recording { point };
                }
                Some(Err(reason)) => {
                    if let Some(action) = branch::action_of(branch_request) {
                        refuse_because(state, action, reason);
                    }
                    continue;
                }
                None => continue,
            },
        }
        state.ops.running = Some(RunningOp {
            id,
            request,
            phase: OpPhase::Running { progress: None },
        });
    }
}

/// Task 20: the undo point and git invocation for a stash request, or why it
/// is refused. A stash is found by its commit id in the fresh snapshot; the
/// position found is what the shell re-checks just before running.
fn plan_stash(
    request: &WriteRequest,
    disk: &DiskState,
    op: OpId,
) -> Result<(UndoPoint, GitArgs), (Action, Refusal)> {
    let stash_ref = stash::stash_ref();
    let before = RefValue {
        oid: undo::current_value(disk, &stash_ref),
        name: stash_ref,
    };
    let find = |action: Action, id: &Oid| {
        stash::position(&disk.stashes, id)
            .map(|index| (index, disk.stashes[index].clone()))
            .ok_or_else(|| (action, Refusal::StashGone(id.clone())))
    };
    let busy = |action: Action| {
        if disk.phase == Phase::Clean {
            Ok(())
        } else {
            Err((action, Refusal::RepoBusy))
        }
    };
    let point = |label: String, refs: Vec<RefValue>, reverse: Reversal| UndoPoint {
        op,
        label,
        head: HeadTarget::of(&disk.head),
        refs,
        index: disk.index.clone(),
        // Only a drop leaves the index alone.
        index_effect: if matches!(request, WriteRequest::StashDrop { .. }) {
            IndexEffect::Untouched
        } else {
            IndexEffect::Rewrites
        },
        reverse,
    };
    let rewrites = Reversal::Never(Irreversible::StashRewroteFiles);
    match request {
        WriteRequest::StashSave {
            message,
            include_untracked,
        } => {
            busy(Action::StashSave)?;
            let message = stash::clean_message(message);
            Ok((
                point(stash::label("Stash", &message), vec![before], rewrites),
                GitArgs::StashSave {
                    message,
                    include_untracked: *include_untracked,
                },
            ))
        }
        WriteRequest::StashApply { stash } => {
            busy(Action::StashApply)?;
            let (index, entry) = find(Action::StashApply, stash)?;
            Ok((
                point(
                    stash::label("Apply stash", &entry.message),
                    Vec::new(),
                    rewrites,
                ),
                GitArgs::StashApply {
                    index,
                    stash: stash.clone(),
                },
            ))
        }
        WriteRequest::StashPop { stash } => {
            busy(Action::StashPop)?;
            let (index, entry) = find(Action::StashPop, stash)?;
            Ok((
                point(
                    stash::label("Pop stash", &entry.message),
                    vec![before],
                    rewrites,
                ),
                GitArgs::StashPop {
                    index,
                    stash: stash.clone(),
                },
            ))
        }
        WriteRequest::StashDrop { stash } => {
            // Dropping touches only the stash list, so a merge or rebase in
            // progress does not matter.
            let (index, entry) = find(Action::StashDrop, stash)?;
            let reverse = if index == 0 {
                Reversal::StashStore(entry.clone())
            } else {
                Reversal::StashKept(entry.clone())
            };
            Ok((
                point(
                    stash::label("Drop stash", &entry.message),
                    vec![before],
                    reverse,
                ),
                GitArgs::StashDrop {
                    index,
                    stash: stash.clone(),
                },
            ))
        }
        // Not reachable (the caller passes stash requests only); refused
        // rather than panicking, because `step` never panics.
        _ => Err((Action::StashSave, Refusal::NotYetSupported { task: 20 })),
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

// Task 23: the conflict flow.

/// Queues a conflict request for the operation paused in the snapshot the
/// user is looking at. `make` may refuse for its own reasons.
fn request_conflict(
    state: &mut RepoState,
    action: Action,
    make: impl FnOnce(Operation) -> Result<WriteRequest, ConflictRefusal>,
) {
    let Some(disk) = state.snapshot.disk() else {
        return refuse_because(state, action, Refusal::NotRead);
    };
    let Some(operation) = disk.phase.operation() else {
        return refuse_conflict(state, action, ConflictRefusal::NotInProgress);
    };
    match make(operation) {
        Ok(request) => enqueue(state, request),
        Err(reason) => refuse_conflict(state, action, reason),
    }
}

/// P2: queues a cherry-pick or revert onto the `HEAD` of the snapshot the
/// user is looking at.
fn request_replay(
    state: &mut RepoState,
    action: Action,
    commit: Oid,
    make: impl FnOnce(Oid, HeadTarget) -> WriteRequest,
) {
    match state.snapshot.disk() {
        None => refuse_because(state, action, Refusal::NotRead),
        Some(disk) => {
            let onto = HeadTarget::of(&disk.head);
            enqueue(state, make(commit, onto));
        }
    }
}

fn refuse_conflict(state: &mut RepoState, action: Action, reason: ConflictRefusal) {
    refuse_because(state, action, Refusal::Conflict(reason));
}

fn operation_name(operation: Operation) -> &'static str {
    match operation {
        Operation::Merge => "merge",
        Operation::Rebase { .. } => "rebase",
        Operation::CherryPick => "cherry-pick",
        Operation::Revert => "revert",
    }
}

// Task 17: network and branch-config requests.

/// The branch checked out in the snapshot the user is looking at, or why
/// there is none.
fn checked_out(state: &RepoState) -> Result<(BranchName, &DiskState), Refusal> {
    let disk = state.snapshot.disk().ok_or(Refusal::NotRead)?;
    match &disk.head {
        Head::Branch { name, .. } => Ok((name.clone(), disk)),
        Head::Detached(_) | Head::Unborn(_) => Err(Refusal::NoBranch),
    }
}

fn still_on(disk: &DiskState, local: &BranchName) -> Result<(), Refusal> {
    match &disk.head {
        Head::Branch { name, .. } if name == local => Ok(()),
        _ => Err(Refusal::BranchChanged),
    }
}

fn request_push(state: &mut RepoState, to: RemoteBranch, force: bool, set_upstream: bool) {
    let (local, disk) = match checked_out(state) {
        Ok(found) => found,
        Err(reason) => return refuse_because(state, Action::Push, reason),
    };
    // Law 10: the lease is the remote value the user is looking at, taken
    // from the snapshot now. If a fetch moves it before the push runs, the
    // remote refuses the push (stale info) instead of overwriting.
    let lease = if force {
        let tracking = to.tracking_ref();
        match disk.refs.refs.get(&tracking) {
            Some(oid) => Some(Lease {
                remote_ref: to.remote_ref(),
                expected: oid.clone(),
            }),
            None => return refuse_because(state, Action::Push, Refusal::NoRemoteValue(tracking)),
        }
    } else {
        None
    };
    enqueue(
        state,
        WriteRequest::Push {
            local,
            to,
            lease,
            set_upstream,
        },
    );
}

fn request_pull(state: &mut RepoState, from: RemoteBranch, mode: PullMode) {
    match checked_out(state) {
        Ok((local, _)) => enqueue(state, WriteRequest::Pull { local, from, mode }),
        Err(reason) => refuse_because(state, Action::Pull, reason),
    }
}

fn request_set_upstream(state: &mut RepoState, upstream: RemoteBranch) {
    match checked_out(state) {
        Ok((local, _)) => enqueue(state, WriteRequest::SetUpstream { local, upstream }),
        Err(reason) => refuse_because(state, Action::SetUpstream, reason),
    }
}

/// Starts a write that takes an undo point (a marker, for push and pull).
fn run_with_point(
    state: &mut RepoState,
    commands: &mut Vec<Command>,
    (op, label): (OpId, String),
    (disk, expect): (&DiskState, Expect),
    args: GitArgs,
    (index_effect, why): (IndexEffect, Irreversible),
) {
    // Both leave a marker an undo stops at (`Reversal::Never`).
    let point = UndoPoint {
        op,
        label,
        head: HeadTarget::of(&disk.head),
        refs: vec![head_ref(&disk.head)],
        index: expect.index.clone(),
        index_effect,
        reverse: Reversal::Never(why),
    };
    commands.push(Command::SnapshotForUndo(point.clone()));
    commands.push(Command::RunGit { op, args, expect });
    state.undo.pending = UndoPending::Recording { point };
}
