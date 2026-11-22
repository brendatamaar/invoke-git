//! Reading the two sides of a file and diffing them, off the UI thread.

use invoke_core::RepoPath;
use shell::precondition::RepoPaths;
use shell::{InputsError, Source, load_inputs};
use ui::{DiffData, diff_view};

/// What the diff viewer shows for one file.
pub enum Loaded {
    Diff(Box<DiffData>),
    /// Binary, not UTF-8, a symlink, a submodule: no text diff exists.
    Message(String),
}

/// Reads `path` for `source` and diffs it. The index can move while the
/// sides are read (the shell reports `Moved`); a few tries are made, since a
/// burst of changes ends quickly.
pub fn load(paths: &RepoPaths, path: &RepoPath, source: Source) -> Loaded {
    for _ in 0..3 {
        match load_inputs(paths, path, source) {
            Ok(inputs) => {
                let old = inputs.old.as_deref().unwrap_or("");
                let new = inputs.new.as_deref().unwrap_or("");
                return Loaded::Diff(Box::new(diff_view::compute(&path.0, old, new)));
            }
            Err(InputsError::Moved) => continue,
            Err(InputsError::NotText(reason)) => {
                return Loaded::Message(format!("{reason}. There is no text diff to show."));
            }
            Err(InputsError::Read(error)) => {
                return Loaded::Message(format!("Could not read {}: {error}", path.0));
            }
        }
    }
    Loaded::Message("The repository kept changing while the file was read. Try again.".into())
}
