//! Undo against a small simulated shell and repo. The simulation answers
//! every command like the real shell would (reads, snapshots, commits,
//! replays) and keeps its own twin of what an undo must do, so the core's
//! decisions are checked against an independent oracle (laws 8, 9, 12).

use std::collections::{BTreeMap, BTreeSet};

use invoke_core::*;
use proptest::collection::vec;
use proptest::prelude::*;

const BRANCHES: [&str; 3] = ["main", "dev", "topic"];

fn oid(n: u64) -> Oid {
    Oid::from_hex(&format!("{n:040x}")).unwrap()
}

fn ref_of(branch: &str) -> RefName {
    BranchName(branch.into()).to_ref()
}

/// The disk of the simulated repo.
#[derive(Clone)]
struct World {
    head: &'static str,
    refs: BTreeMap<RefName, Oid>,
    index: IndexId,
    lock: LockState,
}

impl World {
    fn disk(&self) -> DiskState {
        DiskState {
            head: Head::Branch {
                name: BranchName(self.head.into()),
                oid: self.refs[&ref_of(self.head)].clone(),
            },
            phase: Phase::Clean,
            index: self.index.clone(),
            // Something is always staged, so a plain commit is allowed
            // (task 14).
            status: StatusSnapshot::from_entries([StatusEntry {
                path: RepoPath("a.txt".into()),
                staged: Some(FileChange::Modified),
                unstaged: None,
            }]),
            refs: RefsSnapshot {
                refs: self.refs.clone(),
                ..RefsSnapshot::default()
            },
            stashes: Vec::new(),
            lock: self.lock,
        }
    }
}

/// What the oracle expects an undo to do: one per successful commit.
#[derive(Clone)]
struct Twin {
    branch: &'static str,
    before: Oid,
    after: Oid,
    after_index: IndexId,
}

struct Sim {
    state: RepoState,
    world: World,
    twin: Vec<Twin>,
    counter: u64,
    replays: usize,
    pruned: BTreeSet<OpId>,
    law_8_checked: usize,
}

#[derive(Clone, Debug)]
enum Step {
    Commit {
        amend: bool,
    },
    Undo,
    /// Someone else moves a branch (0 = the current one).
    ExternalMove(usize),
    ExternalIndex,
    Switch(usize),
    Lock(bool),
}

fn action() -> impl Strategy<Value = Step> {
    prop_oneof![
        8 => any::<bool>().prop_map(|amend| Step::Commit { amend }),
        4 => Just(Step::Undo),
        1 => (0..3usize).prop_map(Step::ExternalMove),
        1 => Just(Step::ExternalIndex),
        1 => (0..3usize).prop_map(Step::Switch),
        1 => any::<bool>().prop_map(Step::Lock),
    ]
}

impl Sim {
    fn new() -> Self {
        let refs = BRANCHES
            .iter()
            .enumerate()
            .map(|(i, name)| (ref_of(name), oid(i as u64 + 1)))
            .collect();
        let world = World {
            head: "main",
            refs,
            index: IndexId::Checksum(oid(100)),
            lock: LockState::Free,
        };
        let (state, commands) = open(RepoId {
            root_commit: None,
            remote_url: None,
        });
        let mut sim = Self {
            state,
            world,
            twin: Vec::new(),
            counter: 1000,
            replays: 0,
            pruned: BTreeSet::new(),
            law_8_checked: 0,
        };
        for command in commands {
            sim.exec(command);
        }
        sim
    }

    fn next(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    fn deliver(&mut self, event: Event) {
        let (state, commands) = step(self.state.clone(), event);
        self.state = state;
        for command in commands {
            self.exec(command);
        }
    }

    fn announce(&mut self) {
        self.deliver(Event::DiskChanged(ChangeSet(
            [Area::Refs, Area::Index, Area::Head].into(),
        )));
    }

    fn exec(&mut self, command: Command) {
        match command {
            Command::ReadState { read, .. } => self.deliver(Event::StateRead {
                read,
                disk: self.world.disk(),
            }),
            Command::SnapshotForUndo(point) => {
                assert_eq!(self.world.lock, LockState::Free, "snapshot while locked");
                assert_eq!(point.index, self.world.index, "snapshot on a stale index");
                let tree = oid(self.next());
                self.deliver(Event::UndoSnapshotTaken { op: point.op, tree });
            }
            Command::RunGit { op, expect, .. } => {
                // Law 12: never while someone else holds the lock, and
                // always against what is really on disk.
                assert_eq!(self.world.lock, LockState::Free, "write while locked");
                let disk = self.world.disk();
                assert_eq!(expect.head, disk.head, "write on a stale head");
                assert_eq!(expect.index, disk.index, "write on a stale index");
                let (branch, before) = (
                    self.world.head,
                    self.world.refs[&ref_of(self.world.head)].clone(),
                );
                let after = oid(self.next());
                self.world.refs.insert(ref_of(branch), after.clone());
                self.world.index = IndexId::Checksum(oid(self.next()));
                self.twin.push(Twin {
                    branch,
                    before,
                    after,
                    after_index: self.world.index.clone(),
                });
                if self.twin.len() > 50 {
                    self.twin.remove(0);
                }
                self.deliver(Event::OpFinished(op, OpOutcome::Succeeded));
            }
            Command::ReplayUndo(replay) => {
                assert_eq!(self.world.lock, LockState::Free, "undo while locked");
                assert!(self.oracle_allows(), "undo replayed but the oracle refuses");
                let disk = self.world.disk();
                assert_eq!(replay.expect.head, disk.head);
                assert_eq!(replay.expect.index, disk.index);
                let top = self.twin.pop().unwrap();
                // Law 9: only the recorded branch is touched.
                let touched: Vec<_> = replay.refs.iter().map(|item| &item.name).collect();
                assert_eq!(touched, [&ref_of(top.branch)]);
                for item in &replay.refs {
                    assert_eq!(self.world.refs.get(&item.name), item.expect.as_ref());
                    match &item.restore {
                        Some(value) => self.world.refs.insert(item.name.clone(), value.clone()),
                        None => self.world.refs.remove(&item.name),
                    };
                }
                // Law 8: the branch is exactly where it was before.
                assert_eq!(self.world.refs[&ref_of(top.branch)], top.before);
                self.law_8_checked += 1;
                // Rebuilding the index from a tree gives it a new checksum.
                self.world.index = IndexId::Checksum(oid(self.next()));
                if let Some(below) = self.twin.last_mut() {
                    below.after_index = self.world.index.clone();
                }
                self.replays += 1;
                self.deliver(Event::OpFinished(replay.op, OpOutcome::Succeeded));
            }
            Command::PruneUndo(ops) => self.pruned.extend(ops),
            Command::ApplyPatchToIndex { .. }
            | Command::ApplyPatchToWorktree { .. }
            | Command::CancelOp(_)
            | Command::AnswerAuth(..)
            | Command::RemoveStaleLock => {
                panic!("unexpected command");
            }
        }
    }

    /// Whether an undo is allowed right now, by the twin alone.
    fn oracle_allows(&self) -> bool {
        self.twin.last().is_some_and(|top| {
            top.branch == self.world.head
                && self.world.refs[&ref_of(top.branch)] == top.after
                && self.world.index == top.after_index
        })
    }

    fn apply(&mut self, action: &Step) {
        match action {
            Step::Commit { amend } => self.deliver(Event::CommitRequested {
                message: "msg".into(),
                amend: *amend,
            }),
            Step::Undo => {
                if self.world.lock != LockState::Free || self.state.ops.running.is_some() {
                    return;
                }
                let (allowed, empty, before) =
                    (self.oracle_allows(), self.twin.is_empty(), self.replays);
                self.deliver(Event::UndoRequested);
                assert_eq!(self.replays > before, allowed, "core and oracle disagree");
                if !allowed {
                    let Some(Notice::Refused {
                        action: invoke_core::Action::Undo,
                        reason,
                    }) = &self.state.notice
                    else {
                        panic!("undo neither ran nor was refused: {:?}", self.state.notice);
                    };
                    if empty {
                        assert_eq!(reason, &Refusal::NothingToUndo);
                    } else {
                        assert!(matches!(reason, Refusal::Changed(_)), "{reason:?}");
                    }
                }
            }
            Step::ExternalMove(which) => {
                let branch = if *which == 0 {
                    self.world.head
                } else {
                    BRANCHES[*which]
                };
                let moved = oid(self.next());
                self.world.refs.insert(ref_of(branch), moved);
                self.announce();
            }
            Step::ExternalIndex => {
                self.world.index = IndexId::Checksum(oid(self.next()));
                self.announce();
            }
            Step::Switch(which) => {
                self.world.head = BRANCHES[*which];
                self.announce();
            }
            Step::Lock(held) => {
                self.world.lock = if *held {
                    LockState::HeldByOther
                } else {
                    LockState::Free
                };
                self.announce();
            }
        }
    }

    fn check(&self) {
        // Bounded, and the oldest points are the ones that go.
        assert!(self.state.undo.entries.len() <= 50);
        if self.state.ops.running.is_none() && self.state.ops.queued.is_empty() {
            assert_eq!(self.state.undo.pending, UndoPending::Idle);
            let ops: Vec<_> = self
                .state
                .undo
                .entries
                .iter()
                .map(|entry| (&entry.point.refs, &entry.after_refs))
                .collect();
            assert_eq!(ops.len(), self.twin.len(), "stack and twin differ in depth");
            for (entry, twin) in self.state.undo.entries.iter().zip(&self.twin) {
                assert_eq!(entry.after_refs[0].oid.as_ref(), Some(&twin.after));
                assert_eq!(entry.point.refs[0].oid.as_ref(), Some(&twin.before));
            }
        }
        // A point that is still on the stack is never pruned from disk.
        for entry in &self.state.undo.entries {
            assert!(!self.pruned.contains(&entry.point.op), "live point pruned");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Laws 8, 9 and 12 against the oracle: an undo runs exactly when
    /// nothing moved since the operation, restores exactly the recorded
    /// ref, and nothing is ever written while locked or on a stale
    /// snapshot. The stack is bounded at 50 and mirrors the twin.
    #[test]
    fn undo_matches_the_oracle(actions in vec(action(), 0..200)) {
        let mut sim = Sim::new();
        for action in &actions {
            sim.apply(action);
            sim.check();
        }
        // Release any queued write and look again.
        sim.apply(&Step::Lock(false));
        sim.check();
    }
}

/// Fifty-one commits leave exactly fifty points and prune the oldest.
#[test]
fn the_stack_keeps_fifty_points() {
    let mut sim = Sim::new();
    for _ in 0..(50 + 3) {
        sim.apply(&Step::Commit { amend: false });
    }
    sim.check();
    assert_eq!(sim.state.undo.entries.len(), 50);
    assert_eq!(sim.pruned.len(), 3);
    // The pruned ones are the first three ops (ids 0, 1, 2).
    assert_eq!(sim.pruned, BTreeSet::from([OpId(0), OpId(1), OpId(2)]));
    // Undo walks back through all fifty, then says there is nothing left.
    for _ in 0..50 {
        sim.apply(&Step::Undo);
    }
    assert_eq!(sim.replays, 50);
    sim.apply(&Step::Undo);
    assert_eq!(
        sim.state.notice,
        Some(Notice::Refused {
            action: invoke_core::Action::Undo,
            reason: Refusal::NothingToUndo
        })
    );
}

/// A push on the stack stops an undo with a reason; it never skips past it.
#[test]
fn an_irreversible_entry_stops_undo() {
    let mut sim = Sim::new();
    sim.apply(&Step::Commit { amend: false });
    let mut entry = sim.state.undo.entries.back().unwrap().clone();
    entry.point.op = OpId(99);
    entry.reversible = Reversible::No(Irreversible::Push);
    sim.state.undo.push_irreversible(entry);
    let before = sim.state.undo.clone();
    sim.deliver(Event::UndoRequested);
    assert_eq!(
        sim.state.notice,
        Some(Notice::Refused {
            action: invoke_core::Action::Undo,
            reason: Refusal::NotUndoable(Irreversible::Push)
        })
    );
    assert_eq!(sim.state.undo, before);
    assert_eq!(sim.replays, 0);
}

/// A failed or cancelled operation leaves no undo point behind, and its
/// saved tree is pruned.
#[test]
fn a_failed_commit_leaves_no_point() {
    let mut sim = Sim::new();
    let (state, commands) = step(
        sim.state.clone(),
        Event::CommitRequested {
            message: "x".into(),
            amend: false,
        },
    );
    let op = match commands.as_slice() {
        [Command::SnapshotForUndo(point), Command::RunGit { .. }] => point.op,
        other => panic!("{other:?}"),
    };
    let (state, _) = step(state, Event::UndoSnapshotTaken { op, tree: oid(7) });
    let (state, commands) = step(state, Event::OpFinished(op, OpOutcome::Cancelled));
    assert!(state.undo.entries.is_empty());
    assert_eq!(state.undo.pending, UndoPending::Idle);
    assert!(commands.contains(&Command::PruneUndo(vec![op])));
    sim.state = state;
}
