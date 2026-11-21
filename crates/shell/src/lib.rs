//! Invoke Git's shell: all IO. It runs the core's commands through gix
//! (reads) and the system git CLI (writes and network) and turns every
//! outcome into an event. It holds no business logic; the parsing here is
//! the boundary between git's text and the core's types.

pub mod askpass;
pub mod classify;
pub mod clone;
pub mod discover;
pub mod git;
pub mod precondition;
pub mod progress;
pub mod read;
pub mod runner;
mod tree;
pub mod undo;
pub mod watch;

pub use clone::CloneRunner;
pub use discover::{DiscoverError, Discovered, discover};
pub use git::{Git, GitVersion, SetupError};
pub use precondition::RepoPaths;
pub use read::{execute as execute_read, read_state};
pub use runner::GitRunner;
pub use undo::UndoRunner;
pub use watch::{RepoWatcher, WatchError};
