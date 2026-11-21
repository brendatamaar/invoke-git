//! Invoke Git's shell: all IO. It runs the core's commands through gix
//! (reads) and the system git CLI (writes and network) and turns every
//! outcome into an event. It holds no business logic; the parsing here is
//! the boundary between git's text and the core's types.

pub mod classify;
pub mod git;
pub mod precondition;
pub mod progress;
pub mod runner;
mod tree;

pub use git::{Git, GitVersion, SetupError};
pub use precondition::RepoPaths;
pub use runner::GitRunner;
