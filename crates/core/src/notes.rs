//! Notes on lines of a review (task 22): the data, the rules for what a note
//! may say, and where a note's line is after the file changed. Pure; the
//! store is in `shell`, the screen in `app`.
//!
//! A note is private and local. It is anchored to a commit, a file and a
//! line on the new side of that file's diff, and it remembers the text of the
//! line. A rebase gives every commit a new id, so the commit alone cannot find
//! a note again; the text can: the note is shown where its line text is now,
//! near where it was ([`relocate`]).

/// The id the store gave a note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoteId(pub i64);

/// Where a note was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    /// The commit whose diff was on screen: a single outgoing commit, or the
    /// tip of the branch for the combined diff. Full hex id.
    pub commit: String,
    /// The file, as the repository names it (forward slashes).
    pub path: String,
    /// The line on the new side, counted from 1.
    pub line: u32,
    /// The text of that line when the note was written, without its line
    /// ending: what finds the line again after a rewrite.
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub id: NoteId,
    pub anchor: Anchor,
    /// The branch that was checked out when the note was written. A note
    /// whose commit was rewritten is only re-anchored on this same branch.
    pub branch: String,
    pub body: String,
    /// A to-do: a push warns while it is not resolved.
    pub todo: bool,
    pub resolved: bool,
    /// Unix seconds, given by the shell.
    pub created: i64,
    pub updated: i64,
}

/// The longest note, in characters.
pub const MAX_BODY: usize = 4000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoteProblem {
    Empty,
    TooLong {
        max: usize,
    },
    /// The line is not in the file (counted from 1, and not past its end).
    NoSuchLine {
        lines: usize,
    },
}

/// A note's text as it will be kept: trimmed at both ends, line endings as
/// `\n`, and neither empty nor longer than [`MAX_BODY`].
pub fn clean_body(body: &str) -> Result<String, NoteProblem> {
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    let body = body.trim().to_owned();
    if body.is_empty() {
        return Err(NoteProblem::Empty);
    }
    if body.chars().count() > MAX_BODY {
        return Err(NoteProblem::TooLong { max: MAX_BODY });
    }
    Ok(body)
}

/// The text of line `line` (from 1) of `text`, without its line ending, or
/// why there is no such line.
pub fn line_of(text: &str, line: u32) -> Result<String, NoteProblem> {
    let count = text.lines().count();
    usize::try_from(line)
        .ok()
        .filter(|n| *n >= 1)
        .and_then(|n| text.lines().nth(n - 1))
        .map(str::to_owned)
        .ok_or(NoteProblem::NoSuchLine { lines: count })
}

/// To-dos that are not resolved.
pub fn open_todos<'a>(notes: impl IntoIterator<Item = &'a Note>) -> usize {
    notes
        .into_iter()
        .filter(|note| note.todo && !note.resolved)
        .count()
}

/// Where the note's line is in `text` now (from 1): the same line number if
/// it still holds the same text, else the closest line that does, else
/// `None` (the line is gone, and the note is shown as out of date).
pub fn relocate(anchor: &Anchor, text: &str) -> Option<u32> {
    let wanted = anchor.line as usize;
    let mut best: Option<(usize, usize)> = None; // (distance, line)
    for (index, line) in text.lines().enumerate() {
        if line != anchor.text {
            continue;
        }
        let here = index + 1;
        let distance = here.abs_diff(wanted);
        // Equal distances keep the earlier line, so the answer does not
        // depend on which side is scanned first.
        if best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, here));
        }
    }
    best.and_then(|(_, line)| u32::try_from(line).ok())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn anchor(line: u32, text: &str) -> Anchor {
        Anchor {
            commit: "c".repeat(40),
            path: "a.rs".into(),
            line,
            text: text.into(),
        }
    }

    fn note(todo: bool, resolved: bool) -> Note {
        Note {
            id: NoteId(1),
            anchor: anchor(1, "x"),
            branch: "main".into(),
            body: "b".into(),
            todo,
            resolved,
            created: 0,
            updated: 0,
        }
    }

    #[test]
    fn a_body_is_trimmed_and_never_empty() {
        assert_eq!(clean_body("  check this \r\n"), Ok("check this".into()));
        assert_eq!(clean_body("a\r\nb\rc"), Ok("a\nb\nc".into()));
        assert_eq!(clean_body(" \n\t "), Err(NoteProblem::Empty));
        assert_eq!(
            clean_body(&"x".repeat(MAX_BODY + 1)),
            Err(NoteProblem::TooLong { max: MAX_BODY })
        );
        assert!(clean_body(&"é".repeat(MAX_BODY)).is_ok());
    }

    #[test]
    fn a_line_is_counted_from_one_and_must_exist() {
        let text = "one\ntwo\r\nthree";
        assert_eq!(line_of(text, 1), Ok("one".into()));
        assert_eq!(line_of(text, 2), Ok("two".into()));
        assert_eq!(line_of(text, 3), Ok("three".into()));
        assert_eq!(line_of(text, 0), Err(NoteProblem::NoSuchLine { lines: 3 }));
        assert_eq!(line_of(text, 4), Err(NoteProblem::NoSuchLine { lines: 3 }));
        assert_eq!(line_of("", 1), Err(NoteProblem::NoSuchLine { lines: 0 }));
    }

    #[test]
    fn only_unresolved_todos_count() {
        let notes = [
            note(true, false),
            note(true, true),
            note(false, false),
            note(true, false),
        ];
        assert_eq!(open_todos(&notes), 2);
        assert_eq!(open_todos(&[] as &[Note]), 0);
    }

    #[test]
    fn a_line_is_found_where_it_moved() {
        let a = anchor(3, "let x = 1;");
        // Unmoved, moved down, moved up, gone.
        assert_eq!(relocate(&a, "a\nb\nlet x = 1;\nc"), Some(3));
        assert_eq!(relocate(&a, "new\nnew\na\nb\nlet x = 1;\nc"), Some(5));
        assert_eq!(relocate(&a, "let x = 1;\nc"), Some(1));
        assert_eq!(relocate(&a, "a\nb\nlet x = 2;\n"), None);
        assert_eq!(relocate(&a, ""), None);
    }

    #[test]
    fn the_nearest_of_several_equal_lines_wins() {
        let a = anchor(6, "}");
        let text = "}\nx\n}\nx\nx\nx\nx\n}\n"; // lines 1, 3 and 8
        assert_eq!(relocate(&a, text), Some(8)); // 2 away; line 3 is 3 away
        let a = anchor(5, "}");
        // Lines 3 and 7 are both 2 away: the earlier one.
        assert_eq!(relocate(&a, "x\nx\n}\nx\nx\nx\n}\n"), Some(3));
    }

    proptest! {
        /// A line that is still at its number is found there, and whatever is
        /// found really holds the anchor's text.
        #[test]
        fn what_is_found_holds_the_text(
            lines in proptest::collection::vec("[a-c]{0,2}", 1..20),
            at in 0usize..20,
        ) {
            let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
            let at = at % lines.len();
            let a = anchor(at as u32 + 1, &lines[at]);
            prop_assert_eq!(relocate(&a, &text), Some(at as u32 + 1));
            if let Some(found) = relocate(&anchor(at as u32 + 1, "zzz"), &text) {
                prop_assert_eq!(line_of(&text, found).unwrap(), "zzz");
            } else {
                prop_assert!(!lines.iter().any(|l| l == "zzz"));
            }
        }

        /// Whatever the anchor, no line number outside the file is returned,
        /// and a line that holds the text is always found.
        #[test]
        fn a_match_is_found_if_there_is_one(
            lines in proptest::collection::vec("[a-c]{0,2}", 0..20),
            wanted in 1u32..40,
            text in "[a-c]{0,2}",
        ) {
            let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
            let a = anchor(wanted, &text);
            let exists = lines.contains(&text);
            let found = relocate(&a, &body);
            prop_assert_eq!(found.is_some(), exists);
            if let Some(n) = found {
                prop_assert!(n >= 1 && (n as usize) <= lines.len());
            }
        }
    }
}
