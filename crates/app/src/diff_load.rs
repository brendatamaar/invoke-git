//! Reading the two sides of a file and diffing them, off the UI thread.

use invoke_core::RepoPath;
use shell::precondition::RepoPaths;
use shell::{DiffInputs, InputsError, Source, load_inputs};
use ui::{DiffData, InputsMeta, Section, StageMode, diff_view};

/// What the diff viewer shows for one file.
pub enum Loaded {
    Diff(Box<DiffData>),
    /// A file whose whole-file actions are available without a text diff.
    FileMessage {
        text: String,
        mode: StageMode,
    },
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
fn read(paths: &RepoPaths, path: &RepoPath, section: Section) -> Result<DiffInputs, InputsError> {
    for _ in 0..3 {
        match load_inputs(paths, path, source_of(section)) {
            Ok(inputs) => return Ok(inputs),
            Err(InputsError::Moved) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(InputsError::Moved)
}

/// Reads `path` for `section` and diffs it for the viewer.
pub fn load(paths: &RepoPaths, path: &RepoPath, section: Section) -> Loaded {
    let mode = match section {
        Section::Unstaged => StageMode::Stage,
        Section::Staged => StageMode::Unstage,
    };
    match read(paths, path, section) {
        Ok(inputs) => {
            let mut data = diff_view::compute(
                &path.0,
                inputs.old.as_deref().unwrap_or(""),
                inputs.new.as_deref().unwrap_or(""),
            );
            data.mode = mode;
            data.inputs = Some(meta(&inputs));
            Loaded::Diff(Box::new(data))
        }
        Err(InputsError::NotText(reason)) => {
            let text = format!("{reason}. There is no text diff to show.");
            Loaded::FileMessage { text, mode }
        }
        Err(InputsError::Read(error)) => {
            Loaded::Message(format!("Could not read {}: {error}", path.0))
        }
        Err(InputsError::Moved) => Loaded::Message(
            "The repository kept changing while the file was read. Try again.".into(),
        ),
    }
}
