//! What a discard says before it happens (the design asks to describe what
//! will change, then name the action on the button) and what it is waiting
//! for. Pure: the texts come from the diff as it is shown.

use diff::{LineKind, TextDiff};
use invoke_core::{Event, FileAction, FileRequest, RepoPath};
use shell::discard::FileInputs;
use shell::precondition::RepoPaths;
use ui::InputsMeta;

use crate::staging::Part;

/// A discard waiting for the user's yes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirm {
    /// What will change, in plain words.
    pub text: String,
    /// The danger button: names the action.
    pub button: String,
    pub what: What,
}

/// What a yes does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    /// One request, already built from the diff on screen; the core refuses
    /// it if the index or the file changed meanwhile (rule 5).
    Request(Box<Event>),
    /// Requests read before confirmation and delivered one at a time.
    Files(Vec<Event>),
}

fn lines(count: usize) -> String {
    format!("{count} {}", if count == 1 { "line" } else { "lines" })
}

const UNDO: &str = "You can undo it.";

fn whole_file(path: RepoPath, inputs: FileInputs) -> Confirm {
    let (text, button) = match (inputs.tracked, inputs.file.is_some()) {
        (false, _) => (format!("This deletes {}. {UNDO}", path.0), "Delete file"),
        (true, false) => (
            format!("This brings back {}, which you deleted. {UNDO}", path.0),
            "Restore file",
        ),
        (true, true) => (
            format!("This restores the staged version of {}. {UNDO}", path.0),
            "Discard file",
        ),
    };
    Confirm {
        text,
        button: button.into(),
        what: What::Request(Box::new(file_event(path, inputs))),
    }
}

fn file_event(path: RepoPath, inputs: FileInputs) -> Event {
    Event::FileRequested(FileRequest {
        path,
        action: FileAction::Discard { file: inputs.file },
        based_on: inputs.based_on,
    })
}

pub fn prepare(paths: &RepoPaths, files: Vec<RepoPath>) -> Result<Confirm, String> {
    let mut files = files
        .into_iter()
        .map(|path| {
            let inputs = shell::discard::file_inputs(paths, &path)
                .map_err(|error| format!("Cannot discard {}. {error}", path.0))?;
            Ok((path, inputs))
        })
        .collect::<Result<Vec<_>, String>>()?;
    if files
        .windows(2)
        .any(|pair| pair[0].1.based_on != pair[1].1.based_on)
    {
        return Err("The index changed while preparing the discard. Try again.".into());
    }
    if files.len() == 1 {
        let (path, inputs) = files.remove(0);
        return Ok(whole_file(path, inputs));
    }
    let (text, button) = describe_files(files.len());
    let files = files
        .into_iter()
        .map(|(path, inputs)| file_event(path, inputs))
        .collect();
    Ok(Confirm {
        text,
        button,
        what: What::Files(files),
    })
}

/// The words for discarding `part` of `path`.
pub fn describe(
    path: &RepoPath,
    diff: &TextDiff,
    inputs: &InputsMeta,
    part: &Part,
) -> (String, String) {
    let name = &path.0;
    match part {
        Part::File if !inputs.old_exists => (
            format!("This deletes {name}. Git does not track it yet. {UNDO}"),
            "Delete file".into(),
        ),
        Part::File if !inputs.new_exists => (
            format!("This brings back {name}, which you deleted. {UNDO}"),
            "Restore file".into(),
        ),
        Part::File => (
            format!(
                "This throws away every unstaged change in {name}: {} added, {} removed. {UNDO}",
                lines(diff.stats.added as usize),
                lines(diff.stats.removed as usize),
            ),
            "Discard file".into(),
        ),
        Part::Hunk(hunk) => {
            let (added, removed) = diff.hunks.get(*hunk).map_or((0, 0), |hunk| {
                let count = |kind| hunk.lines.iter().filter(|l| l.kind == kind).count();
                (count(LineKind::Added), count(LineKind::Removed))
            });
            (
                format!(
                    "This throws away one hunk of {name}: {} added, {} removed. {UNDO}",
                    lines(added),
                    lines(removed),
                ),
                "Discard hunk".into(),
            )
        }
        Part::Lines { lines: picked, .. } => (
            format!(
                "This throws away the {} you selected in {name}. {UNDO}",
                lines(picked.len())
            ),
            format!(
                "Discard {}",
                if picked.len() == 1 { "line" } else { "lines" }
            ),
        ),
    }
}

/// The words for discarding the unstaged changes of `count` files.
pub fn describe_files(count: usize) -> (String, String) {
    let files = format!("{count} {}", if count == 1 { "file" } else { "files" });
    (
        format!(
            "This throws away every unstaged change in {files}, and deletes the ones git does \
             not track yet. Undo brings them back one file at a time."
        ),
        format!("Discard {files}"),
    )
}

#[cfg(test)]
mod tests {
    use diff::{DiffOptions, diff_text};
    use invoke_core::IndexId;

    use super::*;

    fn inputs(old: bool, new: bool) -> InputsMeta {
        InputsMeta {
            based_on: IndexId::Absent,
            executable: false,
            old_exists: old,
            new_exists: new,
            new_blob: None,
        }
    }

    fn diff() -> TextDiff {
        diff_text("a\nb\nc\n", "a\nB\nc\nd\n", &DiffOptions::default())
    }

    #[test]
    fn a_file_says_how_many_lines_go() {
        let path = RepoPath("src/f.rs".into());
        let (text, button) = describe(&path, &diff(), &inputs(true, true), &Part::File);
        assert!(text.contains("src/f.rs"), "{text}");
        assert!(text.contains("2 lines added, 1 line removed"), "{text}");
        assert!(text.contains("You can undo it."));
        assert_eq!(button, "Discard file");
    }

    #[test]
    fn an_untracked_file_is_deleted_and_a_deleted_one_comes_back() {
        let path = RepoPath("n.txt".into());
        let (text, button) = describe(&path, &diff(), &inputs(false, true), &Part::File);
        assert!(text.starts_with("This deletes n.txt"), "{text}");
        assert_eq!(button, "Delete file");
        let (text, button) = describe(&path, &diff(), &inputs(true, false), &Part::File);
        assert!(text.contains("brings back n.txt"), "{text}");
        assert_eq!(button, "Restore file");
    }

    #[test]
    fn a_hunk_counts_its_own_lines() {
        let path = RepoPath("f.txt".into());
        let (text, button) = describe(&path, &diff(), &inputs(true, true), &Part::Hunk(0));
        assert!(text.contains("2 lines added, 1 line removed"), "{text}");
        assert_eq!(button, "Discard hunk");
    }

    #[test]
    fn selected_lines_are_counted_and_named_in_the_singular() {
        let path = RepoPath("f.txt".into());
        let one = Part::Lines {
            hunk: 0,
            lines: vec![1],
        };
        let (text, button) = describe(&path, &diff(), &inputs(true, true), &one);
        assert!(text.contains("the 1 line you selected"), "{text}");
        assert_eq!(button, "Discard line");
        let two = Part::Lines {
            hunk: 0,
            lines: vec![1, 2],
        };
        let (text, button) = describe(&path, &diff(), &inputs(true, true), &two);
        assert!(text.contains("the 2 lines you selected"), "{text}");
        assert_eq!(button, "Discard lines");
    }

    #[test]
    fn many_files_say_how_many() {
        assert_eq!(describe_files(1).1, "Discard 1 file");
        assert_eq!(describe_files(5).1, "Discard 5 files");
        assert!(describe_files(5).0.contains("5 files"));
    }
}
