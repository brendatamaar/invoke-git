//! The `proptest-state-machine` wiring: the reference machine only decides
//! which actions may come next (any, in this harness: every selection is
//! resolved against the real repos), and the system under test is the
//! [`World`] with the app and its twin.

use proptest::prelude::*;
use proptest::test_runner::Config;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest};

use crate::transition::{Transition, transition};
use crate::world::World;

/// How many steps a sequence has at most, and how many sequences run, can
/// be set from the environment. CI runs the defaults, the nightly job
/// `INVOKE_HARNESS_CASES=10000`.
pub const DEFAULT_CASES: u32 = 256;
pub const MAX_STEPS: usize = 40;

pub fn cases() -> u32 {
    std::env::var("INVOKE_HARNESS_CASES")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_CASES)
}

pub fn steps() -> usize {
    std::env::var("INVOKE_HARNESS_STEPS")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(MAX_STEPS)
}

/// The config for one of `shards` tests that share the sequences. Each
/// failing sequence is replayed many times while it shrinks, and a replay is
/// a few seconds, so shrinking is bounded.
pub fn config(shards: u32) -> Config {
    Config {
        cases: cases().div_ceil(shards),
        max_shrink_iters: 600,
        max_shrink_time: 180_000,
        // The replay script in the failure message is the artifact; the
        // regression file would only record a seed.
        failure_persistence: None,
        ..Config::default()
    }
}

/// Counts steps only.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub steps: usize,
}

pub struct Reference;

impl ReferenceStateMachine for Reference {
    type State = Plan;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Plan> {
        Just(Plan::default()).boxed()
    }

    fn transitions(_: &Plan) -> BoxedStrategy<Transition> {
        transition().boxed()
    }

    fn apply(state: Plan, _: &Transition) -> Plan {
        Plan {
            steps: state.steps + 1,
        }
    }
}

pub struct Harness;

impl StateMachineTest for Harness {
    type SystemUnderTest = World;
    type Reference = Reference;

    fn init_test(_: &Plan) -> World {
        World::new()
    }

    fn apply(mut world: World, _: &Plan, transition: Transition) -> World {
        world.step(transition);
        world
    }

    fn teardown(_: World, _: Plan) {
        // With INVOKE_HARNESS_STATS set, print how often each kind of step
        // ended each way, every 250 sequences.
        static DONE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let done = DONE.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if done.is_multiple_of(250) && std::env::var_os("INVOKE_HARNESS_STATS").is_some() {
            eprintln!(
                "harness stats after {done} sequences:\n{}",
                crate::world::stats_report()
            );
        }
    }
}
