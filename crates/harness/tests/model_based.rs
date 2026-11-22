//! The model-based harness (backlog task 24; `docs/spec/verification.md`,
//! "Model-based test harness"). It owns laws 4, 5, 6, 8, 9 and 11 and also
//! checks law 12 (no write while `index.lock` is held) on the real shell.
//!
//! Each case is a random sequence of up to 40 user and outside actions. The
//! real core + shell run it on one temp repo and plain git runs it on a
//! twin; the two are compared after every step. A failing sequence is
//! shrunk by proptest and printed as plain git commands.
//!
//! `INVOKE_HARNESS_CASES` sets the number of sequences (default 256, the
//! nightly job uses 10000) and `INVOKE_HARNESS_STEPS` the longest sequence.
//! The sequences are split over two tests so both cores of a CI runner work.

use harness::model::{Harness, config, steps};
use proptest_state_machine::prop_state_machine;

/// The first half of the sequences (the law-ownership table names this one).
mod first_half {
    use super::*;

    prop_state_machine! {
        #![proptest_config(config(2))]

        #[test]
        fn model_based_sequences(sequential 1..steps() => Harness);
    }
}

mod second_half {
    use super::*;

    prop_state_machine! {
        #![proptest_config(config(2))]

        #[test]
        fn model_based_sequences_second_half(sequential 1..steps() => Harness);
    }
}
