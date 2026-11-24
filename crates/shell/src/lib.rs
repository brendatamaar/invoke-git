//! Invoke Git's shell: all IO. It runs the core's commands through gix
//! (reads) and the system git CLI (writes and network) and turns every
//! outcome into an event. It holds no business logic; the parsing here is
//! the boundary between git's text and the core's types.

pub mod advanced;
pub mod askpass;
pub mod blame;
pub mod classify;
pub mod clone;
pub mod commit_detail;
pub mod conflict;
pub mod discard;
pub mod discover;
pub mod git;
pub mod history;
pub mod outgoing;
pub mod precondition;
pub mod progress;
pub mod read;
pub mod rebase;
pub mod reflog;
mod resolve_undo;
pub mod runner;
pub mod search;
pub mod staging;
mod status;
pub mod store;
mod tree;
pub mod undo;
pub mod watch;

pub use blame::{BlameLine, BlameReader, FileCommit, ReadError};
pub use clone::CloneRunner;
pub use conflict::{read_conflict_content, read_worktree_file, write_worktree_file};
pub use discard::DiscardRunner;
pub use discover::{DiscoverError, Discovered, discover};
pub use git::{Git, GitVersion, SetupError};
pub use precondition::{RepoPaths, remove_index_lock, use_user_config};
pub use read::{execute as execute_read, head_message, merge_message, read_state};
pub use reflog::{ReflogEvent, list_reflog_targets, read_reflog, read_reflog_async};
pub use runner::GitRunner;
pub use staging::{DiffInputs, InputsError, Source, StagingRunner, load_inputs};
pub use undo::UndoRunner;
pub use watch::{RepoWatcher, WatchError};
