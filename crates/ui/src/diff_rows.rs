//! A diff as display rows: the layout of the diff viewer, pure.
//!
//! Unified shows one line per row. Split shows two columns: context on both
//! sides, and each block of removed lines next to the block of added lines
//! that replaced it, paired by position (the first removed line beside the
//! first added line), the shorter side padded with blank cells.

use diff::{DiffLine, Hunk, LineKind, TextDiff};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Split,
    Unified,
}

/// One row of the list. Indices point into `diff.hunks[hunk].lines`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// `@@ -a,b +c,d @@` before a hunk.
    HunkHeader {
        hunk: usize,
    },
    Unified {
        hunk: usize,
        line: usize,
    },
    /// At least one side is present. A context line is on both sides of one
    /// row; a removed line is on the left, an added line on the right.
    Split {
        hunk: usize,
        left: Option<usize>,
        right: Option<usize>,
    },
}

/// The rows of `diff` in `layout`.
pub fn rows(diff: &TextDiff, layout: Layout) -> Vec<Row> {
    match layout {
        Layout::Unified => unified_rows(diff),
        Layout::Split => split_rows(diff),
    }
}

pub fn unified_rows(diff: &TextDiff) -> Vec<Row> {
    let mut rows = Vec::new();
    for (hunk, h) in diff.hunks.iter().enumerate() {
        rows.push(Row::HunkHeader { hunk });
        rows.extend((0..h.lines.len()).map(|line| Row::Unified { hunk, line }));
    }
    rows
}

pub fn split_rows(diff: &TextDiff) -> Vec<Row> {
    let mut rows = Vec::new();
    for (hunk, h) in diff.hunks.iter().enumerate() {
        rows.push(Row::HunkHeader { hunk });
        let lines = &h.lines;
        let mut i = 0;
        while i < lines.len() {
            if lines[i].kind == LineKind::Context {
                rows.push(Row::Split {
                    hunk,
                    left: Some(i),
                    right: Some(i),
                });
                i += 1;
                continue;
            }
            // A run of changes: removed on the left, added on the right.
            let (mut removed, mut added) = (Vec::new(), Vec::new());
            while i < lines.len() && lines[i].kind != LineKind::Context {
                match lines[i].kind {
                    LineKind::Removed => removed.push(i),
                    _ => added.push(i),
                }
                i += 1;
            }
            for k in 0..removed.len().max(added.len()) {
                rows.push(Row::Split {
                    hunk,
                    left: removed.get(k).copied(),
                    right: added.get(k).copied(),
                });
            }
        }
    }
    rows
}

/// The hunk header text: `@@ -42,8 +42,12 @@ pub fn build_patch`.
pub fn hunk_header(hunk: &Hunk) -> String {
    let range = |start: u32, len: u32| {
        if len == 1 {
            start.to_string()
        } else {
            format!("{start},{len}")
        }
    };
    let mut text = format!(
        "@@ -{} +{} @@",
        range(hunk.old_start, hunk.old_len),
        range(hunk.new_start, hunk.new_len)
    );
    if let Some(header) = &hunk.header {
        text.push(' ');
        text.push_str(header);
    }
    text
}

/// The text a cell shows for `line`.
pub fn line_text<'a>(diff: &'a TextDiff, line: &DiffLine) -> &'a str {
    diff.line_text(line)
}

#[cfg(test)]
mod tests {
    use diff::{DiffOptions, diff_text};
    use proptest::prelude::*;

    use super::*;

    fn diff_of(old: &str, new: &str, context: u32) -> TextDiff {
        diff_text(
            old,
            new,
            &DiffOptions {
                context,
                ..DiffOptions::default()
            },
        )
    }

    #[test]
    fn a_replaced_line_sits_beside_its_replacement() {
        let diff = diff_of("a\nb\nc\n", "a\nB\nc\n", 3);
        let rows = split_rows(&diff);
        assert_eq!(
            rows,
            vec![
                Row::HunkHeader { hunk: 0 },
                Row::Split {
                    hunk: 0,
                    left: Some(0),
                    right: Some(0)
                },
                Row::Split {
                    hunk: 0,
                    left: Some(1),
                    right: Some(2)
                },
                Row::Split {
                    hunk: 0,
                    left: Some(3),
                    right: Some(3)
                },
            ]
        );
    }

    #[test]
    fn the_shorter_side_is_padded() {
        let diff = diff_of("a\nb\nc\nd\n", "a\nX\nd\n", 0);
        let rows = split_rows(&diff);
        // Two removed (b, c), one added (X): two rows, the second blank on the right.
        let body: Vec<_> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Split { left, right, .. } => Some((*left, *right)),
                _ => None,
            })
            .collect();
        assert_eq!(body, vec![(Some(0), Some(2)), (Some(1), None)]);
    }

    #[test]
    fn identical_texts_have_no_rows() {
        let diff = diff_of("a\nb\n", "a\nb\n", 3);
        assert!(rows(&diff, Layout::Split).is_empty());
        assert!(rows(&diff, Layout::Unified).is_empty());
    }

    #[test]
    fn headers_read_like_git() {
        let diff = diff_of("a\nb\nc\n", "a\nB\nc\n", 3);
        assert_eq!(hunk_header(&diff.hunks[0]), "@@ -1,3 +1,3 @@");
        let one = diff_of("a\n", "b\n", 3);
        assert_eq!(hunk_header(&one.hunks[0]), "@@ -1 +1 @@");
    }

    fn text() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("a"),
                Just("b"),
                Just("c"),
                Just("d"),
                Just(""),
                Just("e")
            ],
            0..14,
        )
        .prop_map(|lines| lines.iter().map(|l| format!("{l}\n")).collect())
    }

    proptest! {
        /// Unified: every diff line is one row, in order, and each hunk has a header.
        #[test]
        fn unified_shows_every_line_once_in_order(old in text(), new in text(), context in 0u32..4) {
            let diff = diff_of(&old, &new, context);
            let rows = unified_rows(&diff);
            let total: usize = diff.hunks.iter().map(|h| h.lines.len() + 1).sum();
            prop_assert_eq!(rows.len(), total);
            let mut expected = Vec::new();
            for (hunk, h) in diff.hunks.iter().enumerate() {
                expected.push(Row::HunkHeader { hunk });
                for line in 0..h.lines.len() {
                    expected.push(Row::Unified { hunk, line });
                }
            }
            prop_assert_eq!(rows, expected);
        }

        /// Split: removed lines are on the left exactly once, added on the
        /// right exactly once, context on both of one row, in order on each
        /// side, and no row is empty.
        #[test]
        fn split_places_each_line_on_its_side_once(old in text(), new in text(), context in 0u32..4) {
            let diff = diff_of(&old, &new, context);
            for (hunk, h) in diff.hunks.iter().enumerate() {
                let mine: Vec<_> = split_rows(&diff)
                    .into_iter()
                    .filter(|r| matches!(r, Row::Split { hunk: x, .. } if *x == hunk))
                    .collect();
                let mut left_seen = Vec::new();
                let mut right_seen = Vec::new();
                for row in &mine {
                    let Row::Split { left, right, .. } = row else { unreachable!() };
                    prop_assert!(left.is_some() || right.is_some(), "an empty row");
                    if let Some(l) = left {
                        prop_assert_ne!(h.lines[*l].kind, LineKind::Added);
                        left_seen.push(*l);
                    }
                    if let Some(r) = right {
                        prop_assert_ne!(h.lines[*r].kind, LineKind::Removed);
                        right_seen.push(*r);
                    }
                    if let (Some(l), Some(r)) = (left, right) {
                        // Both sides of a row: the same context line, or a pair.
                        if h.lines[*l].kind == LineKind::Context {
                            prop_assert_eq!(l, r);
                        }
                    }
                }
                let want_left: Vec<usize> = (0..h.lines.len())
                    .filter(|i| h.lines[*i].kind != LineKind::Added)
                    .collect();
                let want_right: Vec<usize> = (0..h.lines.len())
                    .filter(|i| h.lines[*i].kind != LineKind::Removed)
                    .collect();
                prop_assert_eq!(left_seen, want_left);
                prop_assert_eq!(right_seen, want_right);
            }
        }

        /// Split has one header per hunk, and a run of `r` removed and `a`
        /// added lines takes `max(r, a)` rows.
        #[test]
        fn split_row_count_follows_the_runs(old in text(), new in text(), context in 0u32..4) {
            let diff = diff_of(&old, &new, context);
            let mut want = 0;
            for h in &diff.hunks {
                want += 1;
                let mut i = 0;
                while i < h.lines.len() {
                    if h.lines[i].kind == LineKind::Context {
                        want += 1;
                        i += 1;
                        continue;
                    }
                    let (mut r, mut a) = (0, 0);
                    while i < h.lines.len() && h.lines[i].kind != LineKind::Context {
                        if h.lines[i].kind == LineKind::Removed { r += 1 } else { a += 1 }
                        i += 1;
                    }
                    want += r.max(a);
                }
            }
            prop_assert_eq!(split_rows(&diff).len(), want);
        }
    }
}
