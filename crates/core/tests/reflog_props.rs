//! P7: Reflog types and recovery actions (create branch at OID, reset with
//! expected-ref guard & undo snapshot) in the pure core. Laws 8, 9, 12, 13.

use std::collections::{BTreeMap, BTreeSet};

use invoke_core::reflog::split_reflog_message;
use invoke_core::*;
use proptest::collection::vec;
use proptest::prelude::*;

const BRANCHES: [&str; 3] = ["main", "dev", "topic"];
const RECOVER_NAMES: [&str; 3] = ["recover-1", "recover/a", "backup"];

fn oid(n: u64) -> Oid {
    Oid::from_hex(&format!("{n:040x}")).unwrap()
}

fn branch(name: &str) -> BranchName {
    BranchName(name.into())
}

fn ref_of(name: &str) -> RefName {
    branch(name).to_ref()
}

fn disk_with(
    head: Head,
    phase: Phase,
    status: StatusSnapshot,
    lock: LockState,
    refs: BTreeMap<RefName, Oid>,
    index: IndexId,
) -> DiskState {
    DiskState {
        head,
        phase,
        index,
        status,
        refs: RefsSnapshot {
            refs,
            ..RefsSnapshot::default()
        },
        stashes: Vec::new(),
        lock,
    }
}

fn base_refs() -> BTreeMap<RefName, Oid> {
    BRANCHES
        .iter()
        .enumerate()
        .map(|(i, name)| (ref_of(name), oid(i as u64 + 1)))
        .collect()
}

fn opened(disk: DiskState) -> RepoState {
    let (state, commands) = open(RepoId {
        root_commit: None,
        remote_url: None,
    });
    let Command::ReadState { read, .. } = commands[0] else {
        panic!("open must read first");
    };
    step(state, Event::StateRead { read, disk }).0
}

fn refusal(state: &RepoState) -> Option<(Action, Refusal)> {
    match &state.notice {
        Some(Notice::Refused { action, reason }) => Some((*action, reason.clone())),
        _ => None,
    }
}

fn undo_point(commands: &[Command]) -> Option<UndoPoint> {
    commands.iter().find_map(|c| match c {
        Command::SnapshotForUndo(p) => Some(p.clone()),
        _ => None,
    })
}

fn git_run(commands: &[Command]) -> Option<(OpId, GitArgs, Expect)> {
    commands.iter().find_map(|c| match c {
        Command::RunGit { op, args, expect } => Some((*op, args.clone(), expect.clone())),
        _ => None,
    })
}

fn mode_strategy() -> impl Strategy<Value = ResetMode> {
    prop_oneof![Just(ResetMode::Soft), Just(ResetMode::Mixed)]
}

fn sample_entry(new_id: Oid) -> ReflogEntry {
    ReflogEntry {
        index: 1,
        selector: "HEAD@{1}".into(),
        old_oid: Some(oid(1)),
        new_oid: new_id,
        message: "commit: add feature".into(),
        actor: "Alice".into(),
        email: "alice@example.com".into(),
        seconds: 1_700_000_000,
        offset: 0,
    }
}

// ---------------------------------------------------------------------------
// Simulated shell & independent oracle for reset + recover-branch + undo
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum SimHead {
    Branch(String),
    Detached(Oid),
}

#[derive(Clone)]
struct World {
    head: SimHead,
    refs: BTreeMap<RefName, Oid>,
    index: IndexId,
    lock: LockState,
}

impl World {
    fn disk(&self) -> DiskState {
        let head = match &self.head {
            SimHead::Branch(name) => Head::Branch {
                name: branch(name),
                oid: self.refs[&ref_of(name)].clone(),
            },
            SimHead::Detached(id) => Head::Detached(id.clone()),
        };
        disk_with(
            head,
            Phase::Clean,
            StatusSnapshot::default(),
            self.lock,
            self.refs.clone(),
            self.index.clone(),
        )
    }

    fn head_ref(&self) -> RefName {
        match &self.head {
            SimHead::Branch(name) => ref_of(name),
            SimHead::Detached(_) => RefName("HEAD".into()),
        }
    }

    fn head_target(&self) -> HeadTarget {
        match &self.head {
            SimHead::Branch(name) => HeadTarget::Branch(branch(name)),
            SimHead::Detached(_) => HeadTarget::Detached,
        }
    }

    fn value(&self, name: &RefName) -> Option<Oid> {
        if name.0 == "HEAD" {
            return match &self.head {
                SimHead::Detached(id) => Some(id.clone()),
                SimHead::Branch(_) => None,
            };
        }
        self.refs.get(name).cloned()
    }
}

#[derive(Clone, Debug)]
struct TwinUndo {
    before_head: SimHead,
    after_head: HeadTarget,
    /// `(ref_name, before, after)`
    refs: Vec<(RefName, Option<Oid>, Option<Oid>)>,
    rewrites_index: bool,
    before_index: IndexId,
    after_index: IndexId,
}

#[derive(Clone, Debug)]
enum SimStep {
    Reset {
        target: u64,
        mode: ResetMode,
    },
    RecoverBranch {
        name_idx: usize,
        target: u64,
        switch: bool,
    },
    Undo,
    OutsideMove(usize),
    OutsideIndex,
    OutsideSwitch(usize),
    OutsideDetach(u64),
    Lock(bool),
}

fn sim_step() -> impl Strategy<Value = SimStep> {
    prop_oneof![
        6 => (10u64..30, mode_strategy()).prop_map(|(target, mode)| SimStep::Reset { target, mode }),
        4 => (0..RECOVER_NAMES.len(), 10u64..30, any::<bool>()).prop_map(|(name_idx, target, switch)| {
            SimStep::RecoverBranch { name_idx, target, switch }
        }),
        5 => Just(SimStep::Undo),
        1 => (0..BRANCHES.len()).prop_map(SimStep::OutsideMove),
        1 => Just(SimStep::OutsideIndex),
        1 => (0..BRANCHES.len()).prop_map(SimStep::OutsideSwitch),
        1 => (10u64..30).prop_map(SimStep::OutsideDetach),
        1 => any::<bool>().prop_map(SimStep::Lock),
    ]
}

struct Sim {
    state: RepoState,
    world: World,
    twin: Vec<TwinUndo>,
    saved_trees: BTreeMap<Oid, IndexId>,
    counter: u64,
    resets: usize,
    replays: usize,
}

impl Sim {
    fn new() -> Self {
        let world = World {
            head: SimHead::Branch("main".into()),
            refs: base_refs(),
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
            saved_trees: BTreeMap::new(),
            counter: 1000,
            resets: 0,
            replays: 0,
        };
        for command in commands {
            sim.exec(command);
        }
        sim
    }

    fn next_id(&mut self) -> Oid {
        self.counter += 1;
        oid(self.counter)
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
                assert_eq!(self.world.lock, LockState::Free, "law 12: snapshot locked");
                assert_eq!(point.index, self.world.index, "stale index on snapshot");
                let tree = self.next_id();
                self.saved_trees
                    .insert(tree.clone(), self.world.index.clone());
                self.deliver(Event::UndoSnapshotTaken { op: point.op, tree });
            }
            Command::RunGit { op, args, expect } => {
                assert_eq!(self.world.lock, LockState::Free, "law 12: write locked");
                assert_eq!(expect.head, self.world.disk().head, "stale HEAD");
                assert_eq!(expect.index, self.world.index, "stale index");
                for expected in args.expected_refs() {
                    assert_eq!(
                        self.world.value(&expected.name),
                        expected.oid,
                        "stale ref {:?}",
                        expected.name
                    );
                }
                let before_refs = self.world.refs.clone();
                let before_head = self.world.head.clone();
                let before_index = self.world.index.clone();
                match args {
                    GitArgs::Reset {
                        target,
                        mode,
                        expected_ref,
                    } => {
                        self.resets += 1;
                        let moved_ref = self.world.head_ref();
                        assert_eq!(expected_ref.name, moved_ref);
                        match &mut self.world.head {
                            SimHead::Branch(name) => {
                                self.world.refs.insert(ref_of(name), target.clone());
                            }
                            SimHead::Detached(id) => {
                                *id = target.clone();
                            }
                        }
                        // Law 9: no other branch moved.
                        for (name, before_oid) in &before_refs {
                            if *name != moved_ref {
                                assert_eq!(
                                    self.world.refs.get(name),
                                    Some(before_oid),
                                    "law 9: reset moved unrelated ref {name:?}"
                                );
                            }
                        }
                        let rewrites_index = mode != ResetMode::Soft;
                        if rewrites_index {
                            self.world.index = IndexId::Checksum(self.next_id());
                        }
                        let after_val = self.world.value(&moved_ref);
                        self.twin.push(TwinUndo {
                            before_head,
                            after_head: self.world.head_target(),
                            refs: vec![(moved_ref, expected_ref.oid, after_val)],
                            rewrites_index,
                            before_index,
                            after_index: self.world.index.clone(),
                        });
                        self.deliver(Event::OpFinished(op, OpOutcome::Succeeded));
                    }
                    GitArgs::CreateBranch { name, at, switch } => {
                        let full = name.to_ref();
                        let old_head_ref = self.world.head_ref();
                        let old_head_val = self.world.value(&old_head_ref);
                        let old_head_oid = match &self.world.head {
                            SimHead::Branch(b) => self.world.refs[&ref_of(b)].clone(),
                            SimHead::Detached(id) => id.clone(),
                        };
                        self.world.refs.insert(full.clone(), at.clone());
                        let mut recorded = Vec::new();
                        if switch {
                            recorded.push((old_head_ref.clone(), old_head_val.clone(), None));
                            self.world.head = SimHead::Branch(name.0.clone());
                            let after_old = self.world.value(&old_head_ref);
                            recorded[0].2 = after_old;
                        }
                        recorded.push((full.clone(), None, Some(at.clone())));
                        let rewrites_index = switch && at != old_head_oid;
                        if rewrites_index {
                            self.world.index = IndexId::Checksum(self.next_id());
                        }
                        // Law 9: no existing branch moved.
                        for (r, before_oid) in &before_refs {
                            assert_eq!(self.world.refs.get(r), Some(before_oid));
                        }
                        self.twin.push(TwinUndo {
                            before_head,
                            after_head: self.world.head_target(),
                            refs: recorded,
                            rewrites_index,
                            before_index,
                            after_index: self.world.index.clone(),
                        });
                        self.deliver(Event::OpFinished(op, OpOutcome::Succeeded));
                    }
                    other => panic!("unexpected git args in reflog sim: {other:?}"),
                }
            }
            Command::ReplayUndo(replay) => {
                assert_eq!(self.world.lock, LockState::Free, "law 12: replay locked");
                self.replays += 1;
                let expected = self.twin.pop().expect("replay without twin entry");
                let touched: BTreeSet<RefName> =
                    replay.refs.iter().map(|r| r.name.clone()).collect();
                let before_refs = self.world.refs.clone();
                let head_before_replay = self.world.head.clone();
                if let Some(head_restore) = &replay.head {
                    match head_restore {
                        HeadRestore::Symbolic(name) | HeadRestore::Branch(name) => {
                            self.world.head = SimHead::Branch(name.0.clone());
                        }
                        HeadRestore::Detached(id) => {
                            self.world.head = SimHead::Detached(id.clone());
                        }
                    }
                }
                for item in &replay.refs {
                    assert_eq!(self.world.value(&item.name), item.expect);
                    if item.name.0 == "HEAD" {
                        let restore = item.restore.clone().expect("detached HEAD restore");
                        self.world.head = SimHead::Detached(restore);
                    } else {
                        match &item.restore {
                            Some(id) => {
                                self.world.refs.insert(item.name.clone(), id.clone());
                            }
                            None => {
                                self.world.refs.remove(&item.name);
                            }
                        }
                    }
                }
                if replay.index_effect == IndexEffect::Rewrites {
                    let saved = self.saved_trees[&replay.tree].clone();
                    assert_eq!(saved, expected.before_index, "law 8: saved tree mismatch");
                    // Rebuilding from tree gives a new checksum (re-based on next read).
                    self.world.index = IndexId::Checksum(self.next_id());
                }
                if let Some(prev) = self.twin.last_mut() {
                    prev.after_index = self.world.index.clone();
                }
                // Law 8: HEAD (when touched) and every recorded ref restored exactly.
                let touched_head =
                    replay.head.is_some() || touched.contains(&RefName("HEAD".into()));
                if touched_head {
                    assert_eq!(
                        self.world.head, expected.before_head,
                        "law 8: head mismatch"
                    );
                } else {
                    assert_eq!(self.world.head, head_before_replay, "law 9: HEAD moved");
                }
                for (name, before_val, _) in &expected.refs {
                    assert_eq!(
                        self.world.value(name),
                        *before_val,
                        "law 8: ref {name:?} not restored"
                    );
                }
                // Law 9: no unrecorded ref changed.
                for (name, oid_before) in &before_refs {
                    if !touched.contains(name) {
                        assert_eq!(
                            self.world.refs.get(name),
                            Some(oid_before),
                            "law 9: undo moved unrelated ref {name:?}"
                        );
                    }
                }
                self.deliver(Event::OpFinished(replay.op, OpOutcome::Succeeded));
            }
            Command::PruneUndo(_) => {}
            other => panic!("unexpected command in reflog sim: {other:?}"),
        }
    }

    fn apply(&mut self, step: SimStep) {
        match step {
            SimStep::Reset { target, mode } => {
                self.deliver(Event::ResetRequested {
                    target: oid(target),
                    mode,
                });
                // Every settled undo entry must round-trip through undo_codec (SQLite P1 sync).
                for entry in &self.state.undo.entries {
                    let encoded = undo_codec::encode(entry);
                    assert_eq!(undo_codec::decode(&encoded), Ok(entry.clone()));
                }
            }
            SimStep::RecoverBranch {
                name_idx,
                target,
                switch,
            } => {
                let name = RECOVER_NAMES[name_idx];
                self.deliver(Event::BranchCreateRequested {
                    name: name.into(),
                    at: BranchStart::Commit(oid(target)),
                    switch,
                });
            }
            SimStep::Undo => {
                let can_undo = self.twin.last().is_some_and(|top| {
                    self.world.lock == LockState::Free
                        && self.world.head_target() == top.after_head
                        && top
                            .refs
                            .iter()
                            .all(|(name, _, after)| self.world.value(name) == *after)
                        && (!top.rewrites_index || self.world.index == top.after_index)
                });
                let replays_before = self.replays;
                self.deliver(Event::UndoRequested);
                if self.world.lock == LockState::Free {
                    assert_eq!(
                        self.replays == replays_before + 1,
                        can_undo,
                        "undo decision diverged from oracle"
                    );
                }
            }
            SimStep::OutsideMove(idx) => {
                let name = BRANCHES[idx];
                let next = self.next_id();
                self.world.refs.insert(ref_of(name), next);
                self.announce();
            }
            SimStep::OutsideIndex => {
                let next = self.next_id();
                self.world.index = IndexId::Checksum(next);
                self.announce();
            }
            SimStep::OutsideSwitch(idx) => {
                self.world.head = SimHead::Branch(BRANCHES[idx].into());
                self.announce();
            }
            SimStep::OutsideDetach(target) => {
                self.world.head = SimHead::Detached(oid(target));
                self.announce();
            }
            SimStep::Lock(locked) => {
                self.world.lock = if locked {
                    LockState::HeldByOther
                } else {
                    LockState::Free
                };
                self.announce();
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// `ReflogTarget` selectors and ref names always follow git's spelling.
    #[test]
    fn target_selectors_match_git_format(
        name in "[a-z0-9]{1,8}(/[a-z0-9_-]{1,8}){0,2}",
        index in 0usize..500,
    ) {
        prop_assert_eq!(ReflogTarget::Head.ref_name(), RefName("HEAD".into()));
        prop_assert_eq!(ReflogTarget::Head.selector(index), format!("HEAD@{{{index}}}"));
        let b = BranchName::parse(&name).unwrap();
        let target = ReflogTarget::Branch(b.clone());
        prop_assert_eq!(target.ref_name(), b.to_ref());
        prop_assert_eq!(target.selector(index), format!("{name}@{{{index}}}"));
    }

    /// Splitting a reflog message at `": "` is lossless and never panics.
    #[test]
    fn split_message_is_lossless(msg in "[a-zA-Z0-9: _()/-]{0,60}") {
        let (action, detail) = split_reflog_message(&msg);
        if msg.contains(": ") {
            prop_assert!(!action.contains(": "));
            prop_assert_eq!(format!("{action}: {detail}"), msg);
        } else {
            prop_assert_eq!(action, msg.as_str());
            prop_assert!(detail.is_empty());
        }
    }

    /// `Soft` and `Mixed` resets never rewrite working-tree files, so dirty
    /// worktree/index status never blocks them; every planned reset carries an
    /// expected-ref guard and reversible undo snapshot with the right
    /// `IndexEffect`.
    #[test]
    fn soft_and_mixed_reset_always_plan_guarded_undoable_write(
        mode in mode_strategy(),
        staged in any::<bool>(),
        unstaged in any::<bool>(),
        untracked in any::<bool>(),
    ) {
        let mut entries = Vec::new();
        if staged {
            entries.push(StatusEntry {
                path: RepoPath("staged.txt".into()),
                staged: Some(FileChange::Modified),
                unstaged: None,
            });
        }
        if unstaged {
            entries.push(StatusEntry {
                path: RepoPath("dirty.txt".into()),
                staged: None,
                unstaged: Some(FileChange::Modified),
            });
        }
        if untracked {
            entries.push(StatusEntry {
                path: RepoPath("new.txt".into()),
                staged: None,
                unstaged: Some(FileChange::Untracked),
            });
        }
        let d = disk_with(
            Head::Branch { name: branch("main"), oid: oid(1) },
            Phase::Clean,
            StatusSnapshot::from_entries(entries),
            LockState::Free,
            base_refs(),
            IndexId::Checksum(oid(100)),
        );
        let state = opened(d);
        let (after, commands) = step(state, Event::ResetRequested { target: oid(9), mode });
        prop_assert_eq!(refusal(&after), None);
        let (_, args, expect) = git_run(&commands).expect("must emit RunGit");
        prop_assert_eq!(
            args.expected_refs(),
            vec![RefValue { name: ref_of("main"), oid: Some(oid(1)) }]
        );
        prop_assert_eq!(expect.index, IndexId::Checksum(oid(100)));
        let p = undo_point(&commands).expect("must emit SnapshotForUndo");
        prop_assert_eq!(p.reverse, Reversal::Refs);
        prop_assert_eq!(
            p.index_effect,
            if mode == ResetMode::Soft { IndexEffect::Untouched } else { IndexEffect::Rewrites }
        );
    }

    /// Random sequences of resets, recovery branches, undos, external moves
    /// and locks obey laws 8, 9 and 12 and keep every undo entry codec-safe.
    #[test]
    fn reset_and_recovery_branch_sequences_obey_laws(steps in vec(sim_step(), 1..30)) {
        let mut sim = Sim::new();
        for s in steps {
            sim.apply(s);
        }
        // Release any lock at the end so queued writes drain cleanly.
        if sim.world.lock != LockState::Free {
            sim.apply(SimStep::Lock(false));
        }
        prop_assert!(sim.state.ops.running.is_none());
    }
}

#[test]
fn reset_before_first_read_is_refused_with_not_read() {
    let (state, _) = open(RepoId {
        root_commit: None,
        remote_url: None,
    });
    let (after, commands) = step(
        state,
        Event::ResetRequested {
            target: oid(2),
            mode: ResetMode::Mixed,
        },
    );
    assert_eq!(refusal(&after), Some((Action::Reset, Refusal::NotRead)));
    assert!(commands.iter().all(|c| !c.is_write()));
}

#[test]
fn reset_on_unborn_or_busy_repo_is_refused() {
    let unborn = disk_with(
        Head::Unborn(branch("main")),
        Phase::Clean,
        StatusSnapshot::default(),
        LockState::Free,
        BTreeMap::new(),
        IndexId::Absent,
    );
    for mode in [ResetMode::Soft, ResetMode::Mixed] {
        let (after, commands) = step(
            opened(unborn.clone()),
            Event::ResetRequested {
                target: oid(2),
                mode,
            },
        );
        assert_eq!(
            refusal(&after),
            Some((Action::Reset, Refusal::Branch(BranchRefusal::NoCommitsYet)))
        );
        assert!(commands.iter().all(|c| !c.is_write()));
    }

    for phase in [
        Phase::Merging {
            conflicts: Conflicts::default(),
        },
        Phase::Rebasing {
            step: 1,
            total: 2,
            conflicts: Conflicts::default(),
        },
        Phase::CherryPicking {
            conflicts: Conflicts::default(),
        },
        Phase::Reverting {
            conflicts: Conflicts::default(),
        },
    ] {
        let busy = disk_with(
            Head::Branch {
                name: branch("main"),
                oid: oid(1),
            },
            phase,
            StatusSnapshot::default(),
            LockState::Free,
            base_refs(),
            IndexId::Checksum(oid(100)),
        );
        let (after, commands) = step(
            opened(busy),
            Event::ResetRequested {
                target: oid(2),
                mode: ResetMode::Soft,
            },
        );
        assert_eq!(refusal(&after), Some((Action::Reset, Refusal::RepoBusy)));
        assert!(commands.iter().all(|c| !c.is_write()));
    }
}

#[test]
fn queued_reset_is_refused_if_head_switches_before_dispatch() {
    // Locked repo queues the reset while HEAD is on `main`.
    let locked = disk_with(
        Head::Branch {
            name: branch("main"),
            oid: oid(1),
        },
        Phase::Clean,
        StatusSnapshot::default(),
        LockState::HeldByOther,
        base_refs(),
        IndexId::Checksum(oid(100)),
    );
    let state = opened(locked);
    let (state, commands) = step(
        state,
        Event::ResetRequested {
            target: oid(9),
            mode: ResetMode::Mixed,
        },
    );
    assert!(commands.iter().all(|c| !c.is_write()));
    assert_eq!(state.ops.queued.len(), 1);

    // Before the lock lifts, someone switches to `dev` externally.
    let (state, read_cmds) = step(state, Event::RefreshRequested);
    let Command::ReadState { read, .. } = read_cmds[0] else {
        panic!("expected read");
    };
    let unlocked_on_dev = disk_with(
        Head::Branch {
            name: branch("dev"),
            oid: oid(2),
        },
        Phase::Clean,
        StatusSnapshot::default(),
        LockState::Free,
        base_refs(),
        IndexId::Checksum(oid(100)),
    );
    let (after, commands) = step(
        state,
        Event::StateRead {
            read,
            disk: unlocked_on_dev,
        },
    );
    assert_eq!(
        refusal(&after),
        Some((Action::Reset, Refusal::BranchChanged))
    );
    assert!(commands.iter().all(|c| !c.is_write()));
}

#[test]
fn running_reset_cannot_be_cancelled_but_queued_reset_can() {
    let clean = disk_with(
        Head::Branch {
            name: branch("main"),
            oid: oid(1),
        },
        Phase::Clean,
        StatusSnapshot::default(),
        LockState::Free,
        base_refs(),
        IndexId::Checksum(oid(100)),
    );
    let state = opened(clean);
    let (state, commands) = step(
        state,
        Event::ResetRequested {
            target: oid(2),
            mode: ResetMode::Mixed,
        },
    );
    let (running_op, _, _) = git_run(&commands).expect("first reset runs");
    // Second reset is queued behind the running one.
    let (state, _) = step(
        state,
        Event::ResetRequested {
            target: oid(3),
            mode: ResetMode::Soft,
        },
    );
    let queued_op = state.ops.queued[0].id;

    // Attempting to cancel the running reset emits no CancelOp (law 13).
    let (state, cancel_running_cmds) = step(state, Event::OpCancelRequested(running_op));
    assert!(cancel_running_cmds.is_empty());
    assert!(matches!(
        state.ops.running.as_ref().unwrap().phase,
        OpPhase::Running { .. }
    ));

    // Cancelling the queued reset removes it cleanly.
    let (state, cancel_queued_cmds) = step(state, Event::OpCancelRequested(queued_op));
    assert!(cancel_queued_cmds.is_empty());
    assert!(state.ops.queued.is_empty());
    assert_eq!(
        state.notice,
        Some(Notice::OpEnded {
            op: queued_op,
            outcome: OpOutcome::Cancelled,
        })
    );
}

#[test]
fn reflog_entry_helpers_build_expected_recovery_events() {
    let target = oid(42);
    let entry = sample_entry(target.clone());
    assert_eq!(entry.split_message(), ("commit", "add feature"));
    assert_eq!(
        entry.create_branch_event("recover-feat", true),
        Event::BranchCreateRequested {
            name: "recover-feat".into(),
            at: BranchStart::Commit(target.clone()),
            switch: true,
        }
    );
    assert_eq!(
        entry.reset_event(ResetMode::Soft),
        Event::ResetRequested {
            target,
            mode: ResetMode::Soft,
        }
    );
}
