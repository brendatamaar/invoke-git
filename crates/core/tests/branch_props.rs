//! Branch operations (task 15) against a small simulated shell and repo,
//! with an independent oracle for what is refused, what each operation
//! records and what an undo must restore. Laws 8, 9 and 12.

use std::collections::{BTreeMap, BTreeSet};

use invoke_core::branch;
use invoke_core::*;
use proptest::collection::vec;
use proptest::prelude::*;

/// Includes `topic` and `topic/x`, which cannot exist together.
const POOL: [&str; 5] = ["main", "dev", "topic", "topic/x", "feat"];
const BAD_NAMES: [&str; 4] = ["", "-x", "a b", "a..b"];

fn oid(n: u64) -> Oid {
    Oid::from_hex(&format!("{n:040x}")).unwrap()
}

fn name(n: &str) -> BranchName {
    BranchName(n.into())
}

fn rf(n: &str) -> RefName {
    name(n).to_ref()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WHead {
    Branch(String),
    Detached(Oid),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Branch(&'static str),
    Detached,
}

#[derive(Clone)]
struct World {
    head: WHead,
    refs: BTreeMap<RefName, Oid>,
    index: IndexId,
    lock: LockState,
    busy: bool,
}

impl World {
    fn head_oid(&self) -> Oid {
        match &self.head {
            WHead::Branch(n) => self.refs[&rf(n)].clone(),
            WHead::Detached(o) => o.clone(),
        }
    }

    fn disk(&self) -> DiskState {
        DiskState {
            head: match &self.head {
                WHead::Branch(n) => Head::Branch {
                    name: name(n),
                    oid: self.refs[&rf(n)].clone(),
                },
                WHead::Detached(o) => Head::Detached(o.clone()),
            },
            phase: if self.busy {
                Phase::Merging {
                    conflicts: Conflicts::default(),
                }
            } else {
                Phase::Clean
            },
            index: self.index.clone(),
            status: StatusSnapshot::default(),
            refs: RefsSnapshot {
                refs: self.refs.clone(),
                ..RefsSnapshot::default()
            },
            lock: self.lock,
        }
    }

    /// What a recorded ref holds. `HEAD` is the detached head.
    fn value(&self, n: &RefName) -> Option<Oid> {
        if n.0 == "HEAD" {
            return match &self.head {
                WHead::Detached(o) => Some(o.clone()),
                WHead::Branch(_) => None,
            };
        }
        self.refs.get(n).cloned()
    }

    fn head_ref(&self) -> RefName {
        match &self.head {
            WHead::Branch(n) => rf(n),
            WHead::Detached(_) => RefName("HEAD".into()),
        }
    }

    fn kind(&self) -> Kind {
        match &self.head {
            WHead::Branch(n) => Kind::Branch(POOL.iter().find(|p| **p == n.as_str()).unwrap()),
            WHead::Detached(_) => Kind::Detached,
        }
    }

    fn has(&self, n: &str) -> bool {
        self.refs.contains_key(&rf(n))
    }

    /// A branch that cannot live beside `n`, first in ref order.
    fn conflict(&self, n: &str) -> Option<String> {
        self.refs
            .keys()
            .filter_map(|k| k.0.strip_prefix("refs/heads/"))
            .find(|e| e.starts_with(&format!("{n}/")) || n.starts_with(&format!("{e}/")))
            .map(str::to_owned)
    }
}

/// What the oracle expects an undo of one operation to do.
#[derive(Clone)]
struct Twin {
    /// Recorded refs with the value before and after the operation.
    recorded: Vec<(RefName, Option<Oid>, Option<Oid>)>,
    before_head: WHead,
    after_kind: Kind,
    rewrites: bool,
    after_index: IndexId,
}

#[derive(Clone, Debug)]
enum Step {
    Create {
        name: usize,
        here: bool,
        switch: bool,
    },
    CreateBad(usize),
    Switch(usize),
    Rename(usize, usize),
    Delete(usize, bool),
    Undo,
    OutsideMove(usize),
    OutsideCreate(usize),
    OutsideDelete(usize),
    OutsideIndex,
    OutsideSwitch(usize),
    OutsideDetach,
    Lock(bool),
    Busy(bool),
}

fn action() -> impl Strategy<Value = Step> {
    let n = 0..POOL.len();
    prop_oneof![
        5 => (n.clone(), any::<bool>(), any::<bool>())
            .prop_map(|(name, here, switch)| Step::Create { name, here, switch }),
        1 => (0..BAD_NAMES.len()).prop_map(Step::CreateBad),
        4 => n.clone().prop_map(Step::Switch),
        3 => (n.clone(), n.clone()).prop_map(|(a, b)| Step::Rename(a, b)),
        4 => (n.clone(), any::<bool>()).prop_map(|(a, f)| Step::Delete(a, f)),
        5 => Just(Step::Undo),
        1 => n.clone().prop_map(Step::OutsideMove),
        1 => n.clone().prop_map(Step::OutsideCreate),
        1 => n.clone().prop_map(Step::OutsideDelete),
        1 => Just(Step::OutsideIndex),
        1 => n.prop_map(Step::OutsideSwitch),
        1 => Just(Step::OutsideDetach),
        1 => any::<bool>().prop_map(Step::Lock),
        1 => any::<bool>().prop_map(Step::Busy),
    ]
}

struct Sim {
    state: RepoState,
    world: World,
    twin: Vec<Twin>,
    counter: u64,
    runs: usize,
    replays: usize,
    pruned: BTreeSet<OpId>,
}

impl Sim {
    fn new() -> Self {
        let refs = ["main", "dev", "topic"]
            .iter()
            .enumerate()
            .map(|(i, n)| (rf(n), oid(i as u64 + 1)))
            .collect();
        let world = World {
            head: WHead::Branch("main".into()),
            refs,
            index: IndexId::Checksum(oid(100)),
            lock: LockState::Free,
            busy: false,
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
            runs: 0,
            replays: 0,
            pruned: BTreeSet::new(),
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
            Command::RunGit { op, args, expect } => {
                // Law 12 and rule 5: never while locked, never on a stale
                // snapshot, and every named ref is where the core saw it.
                assert_eq!(self.world.lock, LockState::Free, "write while locked");
                assert!(!self.world.busy, "branch write during a merge");
                let disk = self.world.disk();
                assert_eq!(expect.head, disk.head, "write on a stale head");
                assert_eq!(expect.index, disk.index, "write on a stale index");
                for item in args.expected_refs() {
                    assert_eq!(self.world.value(&item.name), item.oid, "stale ref");
                }
                self.run_git(&args);
                self.runs += 1;
                self.deliver(Event::OpFinished(op, OpOutcome::Succeeded));
            }
            Command::ReplayUndo(replay) => {
                assert_eq!(self.world.lock, LockState::Free, "undo while locked");
                assert!(self.oracle_allows(), "undo replayed but the oracle refuses");
                let disk = self.world.disk();
                assert_eq!(replay.expect.head, disk.head);
                assert_eq!(replay.expect.index, disk.index);
                let top = self.twin.pop().unwrap();
                // Law 9: only recorded refs are touched.
                for item in &replay.refs {
                    assert!(
                        top.recorded.iter().any(|(n, _, _)| *n == item.name),
                        "undo touches {:?}",
                        item.name
                    );
                }
                let head_before_replay = self.world.head.clone();
                // Checkouts first, then the refs, as the shell does.
                match &replay.head {
                    Some(HeadRestore::Symbolic(n) | HeadRestore::Branch(n)) => {
                        self.world.head = WHead::Branch(n.0.clone());
                    }
                    Some(HeadRestore::Detached(o)) => self.world.head = WHead::Detached(o.clone()),
                    None => {}
                }
                for item in &replay.refs {
                    if item.name.0 == "HEAD" {
                        // Only when HEAD stayed detached: moving it.
                        assert_eq!(self.world.value(&item.name), item.expect);
                        self.world.head = WHead::Detached(item.restore.clone().unwrap());
                        continue;
                    }
                    assert_eq!(self.world.refs.get(&item.name), item.expect.as_ref());
                    match &item.restore {
                        Some(v) => self.world.refs.insert(item.name.clone(), v.clone()),
                        None => self.world.refs.remove(&item.name),
                    };
                }
                // Law 8: every recorded ref and HEAD are exactly as before.
                for (n, before, _) in &top.recorded {
                    assert_eq!(&self.world.value(n), before, "{n:?} not restored");
                }
                // HEAD is put back when the operation moved it; otherwise the
                // undo leaves it alone, even if someone detached it at another
                // commit meanwhile.
                if replay.head.is_some() {
                    assert_eq!(self.world.head, top.before_head, "HEAD not restored");
                } else {
                    assert_eq!(self.world.head, head_before_replay, "HEAD was moved");
                }
                if replay.index_effect == IndexEffect::Rewrites {
                    self.world.index = IndexId::Checksum(oid(self.next()));
                }
                if let Some(below) = self.twin.last_mut() {
                    below.after_index = self.world.index.clone();
                }
                self.replays += 1;
                self.deliver(Event::OpFinished(replay.op, OpOutcome::Succeeded));
            }
            Command::PruneUndo(ops) => self.pruned.extend(ops),
            Command::ApplyPatchToIndex { .. } | Command::CancelOp(_) | Command::AnswerAuth(..) => {
                panic!("unexpected command");
            }
        }
    }

    /// Applies a git run to the world, as git would, and records the twin.
    fn run_git(&mut self, args: &GitArgs) {
        let before = self.world.clone();
        let before_value = |n: &RefName| before.value(n);
        let head_oid = before.head_oid();
        let mut rewrites = false;
        let recorded_names: Vec<RefName>;
        let mut moved: BTreeSet<RefName> = BTreeSet::new();
        match args {
            GitArgs::CreateBranch {
                name: n,
                at,
                switch,
            } => {
                assert!(!before.has(&n.0), "create over an existing branch");
                assert_eq!(before.conflict(&n.0), None, "create with a name conflict");
                self.world.refs.insert(n.to_ref(), at.clone());
                moved.insert(n.to_ref());
                recorded_names = if *switch {
                    vec![before.head_ref(), n.to_ref()]
                } else {
                    vec![n.to_ref()]
                };
                if *switch {
                    self.world.head = WHead::Branch(n.0.clone());
                    rewrites = *at != head_oid;
                }
            }
            GitArgs::SwitchBranch { name: n, tip } => {
                assert_ne!(before.head, WHead::Branch(n.0.clone()), "switch to current");
                self.world.head = WHead::Branch(n.0.clone());
                recorded_names = vec![before.head_ref(), n.to_ref()];
                rewrites = *tip != head_oid;
            }
            GitArgs::RenameBranch { from, tip, to } => {
                assert!(!before.has(&to.0) && before.has(&from.0) && from != to);
                assert_eq!(before.conflict(&to.0), None);
                self.world.refs.remove(&from.to_ref());
                self.world.refs.insert(to.to_ref(), tip.clone());
                moved.extend([from.to_ref(), to.to_ref()]);
                if before.head == WHead::Branch(from.0.clone()) {
                    self.world.head = WHead::Branch(to.0.clone());
                }
                recorded_names = vec![from.to_ref(), to.to_ref()];
            }
            GitArgs::DeleteBranch { name: n, .. } => {
                assert_ne!(before.head, WHead::Branch(n.0.clone()), "delete of current");
                self.world.refs.remove(&n.to_ref());
                moved.insert(n.to_ref());
                recorded_names = vec![n.to_ref()];
            }
            GitArgs::Commit { .. } => panic!("no commits in this simulation"),
        }
        if rewrites {
            self.world.index = IndexId::Checksum(oid(self.next()));
        }
        // Law 9: refs that changed are exactly the targeted ones.
        let changed: BTreeSet<RefName> = before
            .refs
            .keys()
            .chain(self.world.refs.keys())
            .filter(|n| before.refs.get(*n) != self.world.refs.get(*n))
            .cloned()
            .collect();
        assert_eq!(changed, moved, "an untargeted branch moved");
        let recorded = recorded_names
            .iter()
            .map(|n| (n.clone(), before_value(n), self.world.value(n)))
            .collect();
        self.twin.push(Twin {
            recorded,
            before_head: before.head.clone(),
            after_kind: self.world.kind(),
            rewrites,
            after_index: self.world.index.clone(),
        });
        if self.twin.len() > 50 {
            self.twin.remove(0);
        }
    }

    /// Whether an undo is allowed right now, by the twin alone.
    fn oracle_allows(&self) -> bool {
        self.twin.last().is_some_and(|top| {
            !self.world.busy
                && top.after_kind == self.world.kind()
                && top
                    .recorded
                    .iter()
                    .all(|(n, _, after)| &self.world.value(n) == after)
                && (!top.rewrites || self.world.index == top.after_index)
        })
    }

    /// What the oracle says a request must be refused with, by the world
    /// alone. `None`: it must run.
    fn oracle_refusal(&self, request: &WriteRequest) -> Option<Refusal> {
        if self.world.busy {
            return Some(Refusal::RepoBusy);
        }
        let w = &self.world;
        let current = |n: &BranchName| w.head == WHead::Branch(n.0.clone());
        let refuse = |why| Some(Refusal::Branch(why));
        match request {
            WriteRequest::CreateBranch { name: n, .. } => {
                if w.has(&n.0) {
                    refuse(BranchRefusal::AlreadyExists(n.clone()))
                } else {
                    w.conflict(&n.0)
                        .and_then(|c| refuse(BranchRefusal::NameConflict(name(&c))))
                }
            }
            WriteRequest::SwitchBranch { name: n } => {
                if !w.has(&n.0) {
                    refuse(BranchRefusal::NoSuchBranch(n.clone()))
                } else if current(n) {
                    refuse(BranchRefusal::IsCurrent(n.clone()))
                } else {
                    None
                }
            }
            WriteRequest::RenameBranch { from, to } => {
                if !w.has(&from.0) {
                    refuse(BranchRefusal::NoSuchBranch(from.clone()))
                } else if from == to {
                    refuse(BranchRefusal::SameName)
                } else if w.has(&to.0) {
                    refuse(BranchRefusal::AlreadyExists(to.clone()))
                } else {
                    w.conflict(&to.0)
                        .and_then(|c| refuse(BranchRefusal::NameConflict(name(&c))))
                }
            }
            WriteRequest::DeleteBranch { name: n, .. } => {
                if !w.has(&n.0) {
                    refuse(BranchRefusal::NoSuchBranch(n.clone()))
                } else if current(n) {
                    refuse(BranchRefusal::IsCurrent(n.clone()))
                } else {
                    None
                }
            }
            _ => unreachable!(),
        }
    }

    /// Sends a branch request and checks the outcome against the oracle.
    fn request(&mut self, event: Event, request: WriteRequest, action: Action) {
        if self.world.lock != LockState::Free || self.state.ops.running.is_some() {
            return;
        }
        let expected = self.oracle_refusal(&request);
        let runs = self.runs;
        self.deliver(event);
        match expected {
            Some(reason) => {
                assert_eq!(self.runs, runs, "refused request ran: {request:?}");
                assert_eq!(
                    self.state.notice,
                    Some(Notice::Refused { action, reason }),
                    "{request:?}"
                );
            }
            None => {
                assert_eq!(
                    self.runs,
                    runs + 1,
                    "valid request did not run: {request:?}"
                );
                assert!(matches!(
                    self.state.notice,
                    Some(Notice::OpEnded {
                        outcome: OpOutcome::Succeeded,
                        ..
                    })
                ));
            }
        }
    }

    fn apply(&mut self, action: &Step) {
        let pool = |i: usize| name(POOL[i]);
        match action {
            Step::Create {
                name: n,
                here,
                switch,
            } => {
                let at = if *here {
                    BranchStart::Head
                } else {
                    BranchStart::Commit(oid(2))
                };
                self.request(
                    Event::BranchCreateRequested {
                        name: POOL[*n].into(),
                        at: at.clone(),
                        switch: *switch,
                    },
                    WriteRequest::CreateBranch {
                        name: pool(*n),
                        at,
                        switch: *switch,
                    },
                    Action::CreateBranch,
                );
            }
            Step::CreateBad(i) => {
                let before = (self.runs, self.state.ops.next);
                self.deliver(Event::BranchCreateRequested {
                    name: BAD_NAMES[*i].into(),
                    at: BranchStart::Head,
                    switch: false,
                });
                assert!(matches!(
                    self.state.notice,
                    Some(Notice::Refused {
                        action: Action::CreateBranch,
                        reason: Refusal::Branch(BranchRefusal::InvalidName(_))
                    })
                ));
                assert_eq!(
                    (self.runs, self.state.ops.next),
                    before,
                    "a bad name was queued"
                );
            }
            Step::Switch(n) => self.request(
                Event::BranchSwitchRequested(pool(*n)),
                WriteRequest::SwitchBranch { name: pool(*n) },
                Action::SwitchBranch,
            ),
            Step::Rename(a, b) => self.request(
                Event::BranchRenameRequested {
                    from: pool(*a),
                    to: POOL[*b].into(),
                },
                WriteRequest::RenameBranch {
                    from: pool(*a),
                    to: pool(*b),
                },
                Action::RenameBranch,
            ),
            Step::Delete(n, force) => self.request(
                Event::BranchDeleteRequested {
                    name: pool(*n),
                    force: *force,
                },
                WriteRequest::DeleteBranch {
                    name: pool(*n),
                    force: *force,
                },
                Action::DeleteBranch,
            ),
            Step::Undo => {
                if self.world.lock != LockState::Free || self.state.ops.running.is_some() {
                    return;
                }
                let (allowed, before) = (self.oracle_allows(), self.replays);
                self.deliver(Event::UndoRequested);
                assert_eq!(self.replays > before, allowed, "core and oracle disagree");
            }
            Step::OutsideMove(n) => {
                if self.world.has(POOL[*n]) {
                    let moved = oid(self.next());
                    self.world.refs.insert(rf(POOL[*n]), moved);
                    self.announce();
                }
            }
            Step::OutsideCreate(n) => {
                if !self.world.has(POOL[*n]) && self.world.conflict(POOL[*n]).is_none() {
                    let at = oid(self.next());
                    self.world.refs.insert(rf(POOL[*n]), at);
                    self.announce();
                }
            }
            Step::OutsideDelete(n) => {
                if self.world.has(POOL[*n]) && self.world.head != WHead::Branch(POOL[*n].into()) {
                    self.world.refs.remove(&rf(POOL[*n]));
                    self.announce();
                }
            }
            Step::OutsideIndex => {
                self.world.index = IndexId::Checksum(oid(self.next()));
                self.announce();
            }
            Step::OutsideSwitch(n) => {
                if self.world.has(POOL[*n]) {
                    self.world.head = WHead::Branch(POOL[*n].into());
                    self.announce();
                }
            }
            Step::OutsideDetach => {
                self.world.head = WHead::Detached(self.world.head_oid());
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
            Step::Busy(busy) => {
                self.world.busy = *busy;
                self.announce();
            }
        }
    }

    fn check(&self) {
        assert!(self.state.undo.entries.len() <= 50);
        if self.state.ops.running.is_none() && self.state.ops.queued.is_empty() {
            assert_eq!(self.state.undo.pending, UndoPending::Idle);
            assert_eq!(
                self.state.undo.entries.len(),
                self.twin.len(),
                "stack and twin differ"
            );
        }
        for entry in &self.state.undo.entries {
            assert!(!self.pruned.contains(&entry.point.op), "live point pruned");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Laws 8, 9 and 12 against the oracle: each request is refused with
    /// the oracle's reason or runs; a run moves only the targeted refs; an
    /// undo runs exactly when the oracle allows it and puts every recorded
    /// ref and HEAD back; nothing is written while locked or stale.
    #[test]
    fn branches_match_the_oracle(actions in vec(action(), 0..120)) {
        let mut sim = Sim::new();
        for action in &actions {
            sim.apply(action);
            sim.check();
        }
        sim.apply(&Step::Lock(false));
        sim.apply(&Step::Busy(false));
        sim.check();
    }
}

fn arb_oid() -> impl Strategy<Value = Oid> {
    (1..8u64).prop_map(oid)
}

/// A random clean disk: some branches, `HEAD` on one of them or detached.
fn arb_disk() -> impl Strategy<Value = DiskState> {
    (
        proptest::collection::btree_map(0..POOL.len(), arb_oid(), 1..5),
        any::<bool>(),
        0..POOL.len(),
        arb_oid(),
    )
        .prop_map(|(branches, detached, pick, at)| {
            let refs: BTreeMap<RefName, Oid> = branches
                .iter()
                .map(|(i, o)| (rf(POOL[*i]), o.clone()))
                .collect();
            let (first, first_oid) = refs
                .iter()
                .next()
                .map(|(k, v)| (k.clone(), v.clone()))
                .unwrap();
            let current = branches
                .keys()
                .copied()
                .find(|i| *i >= pick)
                .map(|i| (POOL[i], refs[&rf(POOL[i])].clone()))
                .unwrap_or((first.0.strip_prefix("refs/heads/").unwrap(), first_oid));
            let head = if detached {
                Head::Detached(at)
            } else {
                Head::Branch {
                    name: name(current.0),
                    oid: current.1,
                }
            };
            DiskState {
                head,
                phase: Phase::Clean,
                index: IndexId::Absent,
                status: StatusSnapshot::default(),
                refs: RefsSnapshot {
                    refs,
                    ..RefsSnapshot::default()
                },
                lock: LockState::Free,
            }
        })
}

proptest! {
    /// Deleting the checked-out branch is refused on every snapshot, forced
    /// or not.
    #[test]
    fn deleting_the_current_branch_is_always_refused(disk in arb_disk(), force in any::<bool>()) {
        if let Head::Branch { name: current, .. } = &disk.head {
            let planned = branch::plan(
                &WriteRequest::DeleteBranch { name: current.clone(), force },
                &disk,
                OpId(0),
            );
            prop_assert_eq!(
                planned.unwrap().err(),
                Some(Refusal::Branch(BranchRefusal::IsCurrent(current.clone())))
            );
        }
    }

    /// A plan that is not refused names exactly the refs it may move, each
    /// with the value the snapshot holds, and records them for undo.
    #[test]
    fn plans_carry_the_snapshots_values(
        disk in arb_disk(),
        a in 0..POOL.len(),
        b in 0..POOL.len(),
        force in any::<bool>(),
        which in 0..4usize,
    ) {
        let request = match which {
            0 => WriteRequest::CreateBranch { name: name(POOL[a]), at: BranchStart::Head, switch: force },
            1 => WriteRequest::SwitchBranch { name: name(POOL[a]) },
            2 => WriteRequest::RenameBranch { from: name(POOL[a]), to: name(POOL[b]) },
            _ => WriteRequest::DeleteBranch { name: name(POOL[a]), force },
        };
        if let Some(Ok((args, point))) = branch::plan(&request, &disk, OpId(3)) {
            for item in args.expected_refs() {
                prop_assert_eq!(disk.refs.refs.get(&item.name), item.oid.as_ref());
            }
            prop_assert_eq!(&point.index, &disk.index);
            prop_assert_eq!(&point.head, &HeadTarget::of(&disk.head));
            // Everything an undo may touch was either named by the run or
            // is the head the run starts from.
            let named: Vec<_> = args.expected_refs().into_iter().map(|r| r.name).collect();
            for item in &point.refs {
                let is_head = match &disk.head {
                    Head::Branch { name, .. } => item.name == name.to_ref(),
                    Head::Detached(_) => item.name.0 == "HEAD",
                    Head::Unborn(_) => false,
                };
                prop_assert!(named.contains(&item.name) || is_head, "{:?}", item.name);
            }
            // Recorded old values are the snapshot's.
            for item in &point.refs {
                let now = if item.name.0 == "HEAD" {
                    match &disk.head { Head::Detached(o) => Some(o), _ => None }
                } else {
                    disk.refs.refs.get(&item.name)
                };
                prop_assert_eq!(now, item.oid.as_ref());
            }
        }
    }

    /// Nothing runs on a repo in the middle of a merge or rebase.
    #[test]
    fn busy_repos_refuse_every_branch_request(disk in arb_disk(), a in 0..POOL.len()) {
        let disk = DiskState { phase: Phase::Merging { conflicts: Conflicts::default() }, ..disk };
        for request in [
            WriteRequest::CreateBranch { name: name("zzz"), at: BranchStart::Head, switch: false },
            WriteRequest::SwitchBranch { name: name(POOL[a]) },
            WriteRequest::RenameBranch { from: name(POOL[a]), to: name("zzz") },
            WriteRequest::DeleteBranch { name: name(POOL[a]), force: true },
        ] {
            let planned = branch::plan(&request, &disk, OpId(0)).unwrap();
            prop_assert_eq!(planned.err(), Some(Refusal::RepoBusy));
        }
    }
}
