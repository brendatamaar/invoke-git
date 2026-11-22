//! Test-only crate (backlog task 24): the laws registry and the
//! model-based harness of `docs/spec/verification.md`.
//!
//! The harness runs random sequences of user and outside actions on the real
//! core and shell (the app) and on plain git in a twin repo, and compares the
//! two after every step. See `tests/model_based.rs` for the entry point and
//! `docs`-level notes in `tasks/024.DONE.md`.

pub mod app;
pub mod content;
pub mod gitdir;
pub mod laws;
pub mod model;
pub mod observe;
pub mod transition;
pub mod world;
