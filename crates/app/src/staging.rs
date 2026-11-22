//! From a selection in the diff viewer to a stage or unstage request. The
//! patch comes from the pure `diff::build_patch`; the request also carries
//! the index id the diff was made against, so the core refuses it when the
//! index moved (rule 5).

use diff::{Direction, Mode, PatchSpec, Pick, TextDiff, build_patch};
use invoke_core::{DiscardRequest, Event, Patch, RepoPath, Selection, StageRequest};
use ui::{InputsMeta, Section};

/// What to stage or unstage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    File,
    Hunk(usize),
    Lines { hunk: usize, lines: Vec<usize> },
}

/// What a selection of a diff turns into: the patch and the selection that
/// names it.
fn build(
    path: &RepoPath,
    diff: &TextDiff,
    inputs: &InputsMeta,
    part: &Part,
    direction: Direction,
) -> Option<(Selection, Patch)> {
    let spec = PatchSpec {
        path: &path.0,
        direction,
        old_exists: inputs.old_exists,
        new_exists: inputs.new_exists,
        mode: if inputs.executable {
            Mode::Executable
        } else {
            Mode::Regular
        },
    };
    let (pick, selection) = match part {
        Part::File => (Pick::File, Selection::File(path.clone())),
        Part::Hunk(hunk) => (
            Pick::Hunk(*hunk),
            Selection::Hunk {
                path: path.clone(),
                hunk: *hunk,
            },
        ),
        Part::Lines { hunk, lines } => (
            Pick::Lines { hunk: *hunk, lines },
            Selection::Lines {
                path: path.clone(),
                hunk: *hunk,
                lines: lines.clone(),
            },
        ),
    };
    let patch = build_patch(diff, &spec, &pick).ok().flatten()?;
    Some((selection, Patch(patch)))
}

/// A discard of `part` of the unstaged changes of `path`. The request
/// carries the index id the diff was made against and the exact bytes of the
/// worktree file it was made from; the core refuses it when either moved
/// (rule 5). `None` when the selection changes nothing.
pub fn discard_request(
    path: &RepoPath,
    diff: &TextDiff,
    inputs: &InputsMeta,
    part: &Part,
) -> Option<Event> {
    let (selection, patch) = build(path, diff, inputs, part, Direction::Discard)?;
    Some(Event::DiscardRequested(DiscardRequest {
        selection,
        patch,
        based_on: inputs.based_on.clone(),
        expect_file: inputs.new_blob.clone(),
    }))
}

/// Builds the event for `part` of `path`, from the diff as it is shown.
/// `None` when the selection changes nothing (only context lines).
pub fn request(
    section: Section,
    path: &RepoPath,
    diff: &TextDiff,
    inputs: &InputsMeta,
    part: &Part,
) -> Option<Event> {
    let direction = match section {
        Section::Unstaged => Direction::Stage,
        Section::Staged => Direction::Unstage,
    };
    let (selection, patch) = build(path, diff, inputs, part, direction)?;
    let request = StageRequest {
        selection,
        patch,
        based_on: inputs.based_on.clone(),
    };
    Some(match section {
        Section::Unstaged => Event::StageRequested(request),
        Section::Staged => Event::UnstageRequested(request),
    })
}

#[cfg(test)]
mod tests {
    use diff::{DiffOptions, diff_text};
    use invoke_core::{IndexId, Oid};

    use super::*;

    fn inputs() -> InputsMeta {
        InputsMeta {
            based_on: IndexId::Absent,
            executable: false,
            old_exists: true,
            new_exists: true,
            new_blob: None,
        }
    }

    fn diff() -> TextDiff {
        diff_text("a\nb\nc\n", "a\nB\nc\nd\n", &DiffOptions::default())
    }

    #[test]
    fn unstaged_files_are_staged_and_staged_files_unstaged() {
        let path = RepoPath("f.txt".into());
        let stage = request(Section::Unstaged, &path, &diff(), &inputs(), &Part::File).unwrap();
        assert!(matches!(stage, Event::StageRequested(_)));
        let unstage = request(Section::Staged, &path, &diff(), &inputs(), &Part::File).unwrap();
        assert!(matches!(unstage, Event::UnstageRequested(_)));
    }

    #[test]
    fn the_request_names_the_index_it_was_made_against() {
        let mut meta = inputs();
        meta.based_on = IndexId::Checksum(Oid::from_hex(&"ab".repeat(20)).unwrap());
        let path = RepoPath("f.txt".into());
        let Some(Event::StageRequested(request)) =
            request(Section::Unstaged, &path, &diff(), &meta, &Part::File)
        else {
            panic!("a stage request")
        };
        assert_eq!(request.based_on, meta.based_on);
    }

    #[test]
    fn selecting_only_context_changes_nothing() {
        let path = RepoPath("f.txt".into());
        // Line 0 of hunk 0 is the context line `a`.
        let part = Part::Lines {
            hunk: 0,
            lines: vec![0],
        };
        assert!(request(Section::Unstaged, &path, &diff(), &inputs(), &part).is_none());
    }

    #[test]
    fn a_discard_carries_what_it_expects_to_find() {
        let mut meta = inputs();
        meta.based_on = IndexId::Checksum(Oid::from_hex(&"ab".repeat(20)).unwrap());
        let blob = Oid::from_hex(&"cd".repeat(20)).unwrap();
        meta.new_blob = Some(blob.clone());
        let path = RepoPath("f.txt".into());
        let Some(Event::DiscardRequested(request)) =
            discard_request(&path, &diff(), &meta, &Part::File)
        else {
            panic!("a discard request")
        };
        assert_eq!(request.based_on, meta.based_on);
        assert_eq!(request.expect_file, Some(blob));
        assert_eq!(request.selection, Selection::File(path));
    }

    #[test]
    fn a_discard_turns_the_selected_added_line_back() {
        let path = RepoPath("f.txt".into());
        let diff = diff();
        let added = diff.hunks[0]
            .lines
            .iter()
            .position(|l| diff.line_text(l) == "d")
            .unwrap();
        let part = Part::Lines {
            hunk: 0,
            lines: vec![added],
        };
        let Some(Event::DiscardRequested(request)) =
            discard_request(&path, &diff, &inputs(), &part)
        else {
            panic!("a discard request")
        };
        // The patch applies to the worktree text: the added line goes away,
        // and the line that was not selected stays as it is.
        assert!(request.patch.0.contains("-d"), "{}", request.patch.0);
        assert!(!request.patch.0.contains("-B"), "{}", request.patch.0);
    }

    #[test]
    fn discarding_only_context_changes_nothing() {
        let path = RepoPath("f.txt".into());
        let part = Part::Lines {
            hunk: 0,
            lines: vec![0],
        };
        assert!(discard_request(&path, &diff(), &inputs(), &part).is_none());
    }
    #[test]
    fn a_line_selection_stages_only_those_lines() {
        let path = RepoPath("f.txt".into());
        let diff = diff();
        let added = diff.hunks[0]
            .lines
            .iter()
            .position(|l| diff.line_text(l) == "d")
            .unwrap();
        let part = Part::Lines {
            hunk: 0,
            lines: vec![added],
        };
        let Some(Event::StageRequested(request)) =
            request(Section::Unstaged, &path, &diff, &inputs(), &part)
        else {
            panic!("a stage request")
        };
        assert!(request.patch.0.contains("+d"));
        assert!(!request.patch.0.contains("+B"));
    }
}
