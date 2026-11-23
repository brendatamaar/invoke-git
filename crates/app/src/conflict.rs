//! The words and small decisions of the conflict screen (task 23, UI half):
//! what the banner says, what each kind of conflict offers, and how a file's
//! text goes in and out of the editor. Pure; `conflict_view` draws it.
//!
//! "Current" is `Ours` (index stage 2, what `HEAD` has) and "incoming" is
//! `Theirs` (stage 3) in every operation, as in the design.

use invoke_core::{ConflictKind, Conflicts, Operation, StageContent};

/// Files with more lines than this are not put in the inline editor.
pub const MAX_EDITOR_LINES: usize = 3000;

pub fn operation_name(operation: Operation) -> &'static str {
    match operation {
        Operation::Merge => "merge",
        Operation::Rebase { .. } => "rebase",
        Operation::CherryPick => "cherry-pick",
        Operation::Revert => "revert",
    }
}

/// `Merging into main`, `Rebasing main: step 2 of 3`.
pub fn title(operation: Operation, head: &str) -> String {
    match operation {
        Operation::Merge => format!("Merging into {head}"),
        Operation::Rebase { step, total } => format!("Rebasing {head}: step {step} of {total}"),
        Operation::CherryPick => format!("Cherry-picking onto {head}"),
        Operation::Revert => format!("Reverting on {head}"),
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The second line of the banner: how far along, and what to do next.
pub fn progress(conflicts: &Conflicts, operation: Operation, head: &str) -> String {
    let total = conflicts.files().len();
    let resolved = conflicts.resolved_count();
    if total == 0 {
        return format!(
            "No file is waiting. Continue the {} to finish it, or abort to put {head} back exactly as it was.",
            operation_name(operation)
        );
    }
    if conflicts.all_resolved() {
        return format!(
            "All {} resolved. Continue the {}, or abort to put {head} back exactly as it was.",
            plural(total, "file", "files"),
            operation_name(operation)
        );
    }
    format!(
        "{resolved} of {} resolved. Resolve every file to continue, or abort to put {head} back exactly as it was.",
        plural(total, "file", "files")
    )
}

pub fn continue_label(operation: Operation) -> String {
    format!("Continue {}", operation_name(operation))
}

pub fn abort_label(operation: Operation) -> String {
    format!("Abort {}", operation_name(operation))
}

/// What aborting costs, asked before it is done.
pub fn abort_question(operation: Operation, head: &str) -> String {
    format!(
        "Abort the {}? {head} goes back exactly as it was before it started, and what you resolved so far is thrown away.",
        operation_name(operation)
    )
}

/// What the two sides are called in this operation.
pub fn side_names(operation: Operation, head: &str) -> (String, String) {
    match operation {
        Operation::Merge => (head.to_owned(), "the branch being merged".to_owned()),
        Operation::Rebase { .. } => (
            "the branch you rebase onto".to_owned(),
            "your commit being applied".to_owned(),
        ),
        Operation::CherryPick => (head.to_owned(), "the picked commit".to_owned()),
        Operation::Revert => (head.to_owned(), "the revert".to_owned()),
    }
}

/// `1 line`, `4 lines`.
pub fn lines_text(count: usize) -> String {
    plural(count, "line", "lines")
}

/// A few words for the file list.
pub fn kind_text(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::BothModified => "Both changed",
        ConflictKind::BothAdded => "Both added",
        ConflictKind::DeletedByUs => "Deleted in current",
        ConflictKind::DeletedByThem => "Deleted in incoming",
        ConflictKind::AddedByUs => "Added in current",
        ConflictKind::AddedByThem => "Added in incoming",
        ConflictKind::BothDeleted => "Both deleted",
    }
}

/// Why a file is not resolved in the editor, and what the choices are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// A side deleted or added the file; there is nothing to merge.
    OneSided(ConflictKind),
    Binary,
    /// The file is longer than [`MAX_EDITOR_LINES`].
    TooLarge,
    /// The file mixes `\n` and `\r\n`; editing it here would change bytes the
    /// user did not touch.
    MixedEndings,
    /// There is no file in the working tree to merge.
    Missing,
}

/// How a conflicted file is resolved on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The editor, with this line ending put back on save.
    Editor(Eol),
    /// Whole-file choices only.
    Sides(Why),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    Lf,
    Crlf,
}

/// Chooses how to resolve a file from its kind and what is in the working
/// tree now.
pub fn mode_for(kind: ConflictKind, worktree: &StageContent) -> Mode {
    if !matches!(kind, ConflictKind::BothModified | ConflictKind::BothAdded) {
        return Mode::Sides(Why::OneSided(kind));
    }
    match worktree {
        StageContent::Absent => Mode::Sides(Why::Missing),
        StageContent::Binary { .. } => Mode::Sides(Why::Binary),
        StageContent::Text(text) => {
            if text.lines().count() > MAX_EDITOR_LINES {
                return Mode::Sides(Why::TooLarge);
            }
            match eol_of(text) {
                Some(eol) => Mode::Editor(eol),
                None => Mode::Sides(Why::MixedEndings),
            }
        }
    }
}

/// The one line ending the text uses; `None` when it mixes both. A text with
/// no line break counts as `\n`.
pub fn eol_of(text: &str) -> Option<Eol> {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count();
    match (crlf, lf - crlf) {
        (0, _) => Some(Eol::Lf),
        (_, 0) => Some(Eol::Crlf),
        _ => None,
    }
}

/// The text as the editor holds it: `\n` only.
pub fn to_editor(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// The editor's text as it is written back.
pub fn from_editor(text: &str, eol: Eol) -> String {
    match eol {
        Eol::Lf => text.to_owned(),
        Eol::Crlf => text.replace('\n', "\r\n"),
    }
}

/// The sentence for a file that has no editor.
pub fn why_text(why: &Why, current: &str, incoming: &str) -> String {
    match why {
        Why::OneSided(kind) => match kind {
            ConflictKind::DeletedByUs => format!(
                "{current} deleted this file and {incoming} changed it. Keep the deletion or keep the changed file."
            ),
            ConflictKind::DeletedByThem => format!(
                "{incoming} deleted this file and {current} changed it. Keep the file or delete it."
            ),
            ConflictKind::AddedByUs => format!(
                "{current} added this file and {incoming} has no such file. Keep it or leave it out."
            ),
            ConflictKind::AddedByThem => format!(
                "{incoming} added this file and {current} has no such file. Keep it or leave it out."
            ),
            ConflictKind::BothDeleted => {
                "Both sides deleted this file. Confirm the deletion.".to_owned()
            }
            ConflictKind::BothModified | ConflictKind::BothAdded => {
                "Choose one version of this file.".to_owned()
            }
        },
        Why::Binary => "This is a binary file. Git cannot merge it; choose one version.".to_owned(),
        Why::TooLarge => format!(
            "This file has more than {MAX_EDITOR_LINES} lines, too many for the editor here. Choose one version, or fix it in your editor and then mark it resolved."
        ),
        Why::MixedEndings => "This file mixes Windows and Unix line endings, so editing it here could change lines you did not touch. Choose one version, or fix it in your editor and then mark it resolved.".to_owned(),
        Why::Missing => "The file is not in the working tree. Choose one version.".to_owned(),
    }
}

/// The two whole-file buttons for a kind: `(current, incoming)`.
pub fn side_buttons(kind: ConflictKind) -> (&'static str, &'static str) {
    match kind {
        ConflictKind::DeletedByUs => ("Keep it deleted", "Keep the incoming file"),
        ConflictKind::DeletedByThem => ("Keep the current file", "Delete the file"),
        ConflictKind::AddedByUs => ("Keep the current file", "Leave it out"),
        ConflictKind::AddedByThem => ("Leave it out", "Keep the incoming file"),
        ConflictKind::BothDeleted => ("Confirm the deletion", "Confirm the deletion"),
        ConflictKind::BothModified | ConflictKind::BothAdded => {
            ("Keep the current version", "Keep the incoming version")
        }
    }
}

#[cfg(test)]
mod tests {
    use invoke_core::RepoPath;
    use proptest::prelude::*;

    use super::*;

    fn conflicts(open: usize, done: usize) -> Conflicts {
        Conflicts::from_parts(
            (0..open).map(|n| (RepoPath(format!("u{n}")), ConflictKind::BothModified)),
            (0..done).map(|n| RepoPath(format!("r{n}"))),
        )
    }

    #[test]
    fn the_banner_names_the_operation_and_the_branch() {
        assert_eq!(title(Operation::Merge, "main"), "Merging into main");
        assert_eq!(
            title(Operation::Rebase { step: 2, total: 3 }, "topic"),
            "Rebasing topic: step 2 of 3"
        );
        assert_eq!(
            title(Operation::CherryPick, "main"),
            "Cherry-picking onto main"
        );
        assert_eq!(continue_label(Operation::Revert), "Continue revert");
        assert_eq!(lines_text(1), "1 line");
        assert_eq!(lines_text(4), "4 lines");
        assert_eq!(abort_label(Operation::Merge), "Abort merge");
    }

    #[test]
    fn the_progress_line_counts_files_and_says_what_is_next() {
        let line = |open, done| progress(&conflicts(open, done), Operation::Merge, "main");
        assert_eq!(
            line(1, 1),
            "1 of 2 files resolved. Resolve every file to continue, or abort to put main back exactly as it was."
        );
        assert!(line(0, 2).starts_with("All 2 files resolved. Continue the merge"));
        assert!(line(0, 1).starts_with("All 1 file resolved."));
        assert!(line(0, 0).starts_with("No file is waiting."));
    }

    #[test]
    fn a_text_file_with_both_sides_changed_gets_the_editor() {
        let text = |s: &str| StageContent::Text(s.to_owned());
        assert_eq!(
            mode_for(ConflictKind::BothModified, &text("a\nb\n")),
            Mode::Editor(Eol::Lf)
        );
        assert_eq!(
            mode_for(ConflictKind::BothAdded, &text("a\r\nb\r\n")),
            Mode::Editor(Eol::Crlf)
        );
        assert_eq!(
            mode_for(ConflictKind::BothModified, &text("a\r\nb\n")),
            Mode::Sides(Why::MixedEndings)
        );
        assert_eq!(
            mode_for(
                ConflictKind::BothModified,
                &StageContent::Binary { size: 4 }
            ),
            Mode::Sides(Why::Binary)
        );
        assert_eq!(
            mode_for(ConflictKind::BothModified, &StageContent::Absent),
            Mode::Sides(Why::Missing)
        );
        let long = "x\n".repeat(MAX_EDITOR_LINES + 1);
        assert_eq!(
            mode_for(ConflictKind::BothModified, &text(&long)),
            Mode::Sides(Why::TooLarge)
        );
        assert_eq!(
            mode_for(ConflictKind::DeletedByUs, &text("a\n")),
            Mode::Sides(Why::OneSided(ConflictKind::DeletedByUs))
        );
    }

    #[test]
    fn the_sides_are_named_for_the_operation() {
        assert_eq!(
            side_names(Operation::Merge, "main"),
            ("main".to_owned(), "the branch being merged".to_owned())
        );
        let (current, incoming) =
            side_names(Operation::Rebase { step: 1, total: 2 }, "your branch");
        assert_eq!(current, "the branch you rebase onto");
        assert_eq!(incoming, "your commit being applied");
    }

    #[test]
    fn every_kind_has_words_and_two_buttons() {
        for kind in [
            ConflictKind::BothModified,
            ConflictKind::BothAdded,
            ConflictKind::DeletedByUs,
            ConflictKind::DeletedByThem,
            ConflictKind::AddedByUs,
            ConflictKind::AddedByThem,
            ConflictKind::BothDeleted,
        ] {
            assert!(!kind_text(kind).is_empty());
            let (a, b) = side_buttons(kind);
            assert!(!a.is_empty() && !b.is_empty());
            assert!(!why_text(&Why::OneSided(kind), "main", "feature").is_empty());
        }
        let (current, incoming) = ("main", "the other");
        let text = why_text(&Why::OneSided(ConflictKind::DeletedByUs), current, incoming);
        assert!(text.starts_with("main deleted this file and the other changed it"));
    }

    proptest! {
        /// Text with one kind of line ending goes into the editor and comes
        /// back byte for byte; a text with both is refused.
        #[test]
        fn line_endings_survive_the_editor(
            lines in proptest::collection::vec("[a-z ]{0,6}", 0..8),
            crlf: bool,
        ) {
            let eol = if crlf { "\r\n" } else { "\n" };
            let text: String = lines.iter().map(|l| format!("{l}{eol}")).collect();
            let kind = eol_of(&text).expect("one kind of ending");
            prop_assert_eq!(kind, if crlf && !text.is_empty() { Eol::Crlf } else { Eol::Lf });
            let in_editor = to_editor(&text);
            prop_assert!(!in_editor.contains('\r'));
            prop_assert_eq!(from_editor(&in_editor, kind), text);
        }

        #[test]
        fn a_mix_is_never_edited_inline(a in "[a-z]{1,4}", b in "[a-z]{1,4}") {
            let text = format!("{a}\r\n{b}\n");
            prop_assert_eq!(eol_of(&text), None);
            prop_assert_eq!(
                mode_for(ConflictKind::BothModified, &StageContent::Text(text)),
                Mode::Sides(Why::MixedEndings)
            );
        }
    }
}
