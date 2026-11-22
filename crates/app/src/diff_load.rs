//! Reading the two sides of a file and diffing them, off the UI thread.

use diff::{DiffOptions, diff_text};
use invoke_core::{Event, RepoPath};
use shell::precondition::RepoPaths;
use shell::{DiffInputs, InputsError, Source, load_inputs};
use ui::{DiffData, InputsMeta, Section, StageMode, diff_view};

use crate::staging::{self, Part};

/// What the diff viewer shows for one file.
pub enum Loaded {
    Diff(Box<DiffData>),
    /// Binary, not UTF-8, a symlink, a submodule: no text diff exists.
    Message(String),
}

fn source_of(section: Section) -> Source {
    match section {
        Section::Unstaged => Source::IndexToWorktree,
        Section::Staged => Source::HeadToIndex,
    }
}

fn meta(inputs: &DiffInputs) -> InputsMeta {
    InputsMeta {
        based_on: inputs.based_on.clone(),
        executable: inputs.executable,
        old_exists: inputs.old.is_some(),
        new_exists: inputs.new.is_some(),
        new_blob: inputs.new_blob.clone(),
    }
}

/// Reads the sides, retrying a few times when the index moves while they are
/// read (a burst of changes ends quickly).
fn read(paths: &RepoPaths, path: &RepoPath, section: Section) -> Result<DiffInputs, String> {
    for _ in 0..3 {
        match load_inputs(paths, path, source_of(section)) {
            Ok(inputs) => return Ok(inputs),
            Err(InputsError::Moved) => continue,
            Err(InputsError::NotText(reason)) => {
                return Err(format!("{reason}. There is no text diff to show."));
            }
            Err(InputsError::Read(error)) => {
                return Err(format!("Could not read {}: {error}", path.0));
            }
        }
    }
    Err("The repository kept changing while the file was read. Try again.".into())
}

/// Reads `path` for `section` and diffs it for the viewer.
pub fn load(paths: &RepoPaths, path: &RepoPath, section: Section) -> Loaded {
    match read(paths, path, section) {
        Ok(inputs) => {
            let mut data = diff_view::compute(
                &path.0,
                inputs.old.as_deref().unwrap_or(""),
                inputs.new.as_deref().unwrap_or(""),
            );
            data.mode = match section {
                Section::Unstaged => StageMode::Stage,
                Section::Staged => StageMode::Unstage,
            };
            data.inputs = Some(meta(&inputs));
            Loaded::Diff(Box::new(data))
        }
        Err(message) => Loaded::Message(message),
    }
}

/// What to do with a whole file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Stage it (from the unstaged section) or unstage it (from the staged one).
    Toggle,
    /// Throw away its unstaged changes.
    Discard,
}

/// The request for the whole file, built from a fresh read of its sides.
/// `Ok(None)`: the file has nothing to do.
pub fn file_request(
    paths: &RepoPaths,
    path: &RepoPath,
    section: Section,
    verb: Verb,
) -> Result<Option<Event>, String> {
    let inputs = read(paths, path, section)?;
    let diff = diff_text(
        inputs.old.clone().unwrap_or_default(),
        inputs.new.clone().unwrap_or_default(),
        &DiffOptions::default(),
    );
    Ok(match verb {
        Verb::Toggle => staging::request(section, path, &diff, &meta(&inputs), &Part::File),
        Verb::Discard => staging::discard_request(path, &diff, &meta(&inputs), &Part::File),
    })
}
