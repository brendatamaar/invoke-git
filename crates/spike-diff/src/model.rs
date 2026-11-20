//! Loading, diffing and split-view row alignment. Spike quality: the real
//! engine is backlog task 8.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use imara_diff::{Algorithm, Diff, InternedInput};

/// One side of the diff: the whole text (for tree-sitter) plus per-line views.
pub struct SideText {
    pub text: Arc<str>,
    /// Byte offset where each line starts in `text`.
    pub line_starts: Vec<usize>,
    /// Byte range of each line in `text`, without its line ending. Strings for
    /// GPUI are made only for visible rows, which keeps first paint cheap.
    pub lines: Vec<std::ops::Range<usize>>,
}

impl SideText {
    pub fn new(text: String) -> Self {
        let text: Arc<str> = text.into();
        let mut line_starts = Vec::new();
        let mut lines = Vec::new();
        let mut start = 0;
        for line in text.split_inclusive('\n') {
            let body = line.strip_suffix('\n').unwrap_or(line);
            let body = body.strip_suffix('\r').unwrap_or(body);
            line_starts.push(start);
            lines.push(start..start + body.len());
            start += line.len();
        }
        Self {
            text,
            line_starts,
            lines,
        }
    }

    /// Line `index` without its line ending.
    pub fn line(&self, index: usize) -> &str {
        &self.text[self.lines[index].clone()]
    }

    /// Byte range of lines `start..end` (end exclusive) in `text`.
    pub fn byte_range(&self, start: usize, end: usize) -> std::ops::Range<usize> {
        let from = self
            .line_starts
            .get(start)
            .copied()
            .unwrap_or(self.text.len());
        let to = self
            .line_starts
            .get(end)
            .copied()
            .unwrap_or(self.text.len());
        from..to
    }
}

/// One row of the split view. Line numbers are 0-based indices into each side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// Unchanged line, shown on both sides.
    Context { old: u32, new: u32 },
    /// Part of a hunk: a removed line on the left and/or an added line on the
    /// right. `None` is filler where one side of the hunk is shorter.
    Change { old: Option<u32>, new: Option<u32> },
}

pub struct Loaded {
    pub old: Arc<SideText>,
    pub new: Arc<SideText>,
    pub rows: Arc<Vec<Row>>,
    pub added: u32,
    pub removed: u32,
    /// Both files are in memory: the start of the first-paint budget.
    pub data_ready: Instant,
    pub read_time: Duration,
    /// Splitting both sides into lines.
    pub index_time: Duration,
    pub diff_time: Duration,
    pub rows_time: Duration,
}

/// Reads both files, then diffs them. Runs on a background thread.
pub fn load(old_path: &Path, new_path: &Path) -> Result<Loaded, String> {
    let started = Instant::now();
    let read = |path: &Path| {
        std::fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))
    };
    let (old, new) = (read(old_path)?, read(new_path)?);
    let data_ready = Instant::now();
    let read_time = data_ready - started;

    let (old, new) = (SideText::new(old), SideText::new(new));
    let indexed = Instant::now();
    let input = InternedInput::new(old.text.as_ref(), new.text.as_ref());
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    let diffed = Instant::now();
    let hunks = diff.hunks().map(|h| (h.before, h.after));
    let rows = build_rows(old.lines.len() as u32, new.lines.len() as u32, hunks);
    let done = Instant::now();

    Ok(Loaded {
        index_time: indexed - data_ready,
        diff_time: diffed - indexed,
        rows_time: done - diffed,
        added: diff.count_additions(),
        removed: diff.count_removals(),
        old: Arc::new(old),
        new: Arc::new(new),
        rows: Arc::new(rows),
        data_ready,
        read_time,
    })
}

/// Aligns a hunk list into split-view rows: context on both sides, and inside
/// each hunk removed lines paired with added lines by position.
pub fn build_rows(
    old_len: u32,
    new_len: u32,
    hunks: impl IntoIterator<Item = (std::ops::Range<u32>, std::ops::Range<u32>)>,
) -> Vec<Row> {
    let mut rows = Vec::with_capacity(old_len.max(new_len) as usize);
    let (mut old, mut new) = (0, 0);
    for (before, after) in hunks {
        while old < before.start {
            rows.push(Row::Context { old, new });
            old += 1;
            new += 1;
        }
        let (removed, added) = (before.len() as u32, after.len() as u32);
        for i in 0..removed.max(added) {
            rows.push(Row::Change {
                old: (i < removed).then_some(before.start + i),
                new: (i < added).then_some(after.start + i),
            });
        }
        old = before.end;
        new = after.end;
    }
    while old < old_len {
        rows.push(Row::Context { old, new });
        old += 1;
        new += 1;
    }
    debug_assert_eq!(new, new_len, "context must end both sides together");
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_for(old: &str, new: &str) -> (SideText, SideText, Vec<Row>) {
        let (old, new) = (SideText::new(old.into()), SideText::new(new.into()));
        let input = InternedInput::new(old.text.as_ref(), new.text.as_ref());
        let diff = Diff::compute(Algorithm::Histogram, &input);
        let hunks = diff.hunks().map(|h| (h.before, h.after));
        let rows = build_rows(old.lines.len() as u32, new.lines.len() as u32, hunks);
        (old, new, rows)
    }

    /// Every line of each side appears exactly once, in order, and context
    /// rows pair identical lines.
    fn assert_aligned(old: &SideText, new: &SideText, rows: &[Row]) {
        let (mut olds, mut news) = (Vec::new(), Vec::new());
        for row in rows {
            match *row {
                Row::Context { old: o, new: n } => {
                    assert_eq!(old.line(o as usize), new.line(n as usize));
                    olds.push(o);
                    news.push(n);
                }
                Row::Change { old: o, new: n } => {
                    assert!(o.is_some() || n.is_some(), "empty change row");
                    olds.extend(o);
                    news.extend(n);
                }
            }
        }
        assert_eq!(olds, (0..old.lines.len() as u32).collect::<Vec<_>>());
        assert_eq!(news, (0..new.lines.len() as u32).collect::<Vec<_>>());
    }

    #[test]
    fn rows_cover_both_sides_in_order() {
        let cases = [
            ("a\nb\nc\n", "a\nb\nc\n"),
            ("a\nb\nc\n", "a\nx\nc\n"),
            ("a\nb\nc\n", "a\nc\n"),
            ("a\nc\n", "a\nb\nb2\nc\n"),
            ("", "a\nb\n"),
            ("a\nb\n", ""),
            ("a\nb\nc\nd\n", "x\nb\ny\nz\nd\nw\n"),
        ];
        for (old, new) in cases {
            let (old, new, rows) = rows_for(old, new);
            assert_aligned(&old, &new, &rows);
        }
    }

    #[test]
    fn fixture_sized_input_aligns() {
        let old: String = (0..5_000).map(|i| format!("line {i}\n")).collect();
        let new: String = (0..5_000)
            .filter(|i| i % 97 != 0)
            .map(|i| match i % 41 {
                0 => format!("changed {i}\nextra {i}\n"),
                _ => format!("line {i}\n"),
            })
            .collect();
        let (old, new, rows) = rows_for(&old, &new);
        assert_aligned(&old, &new, &rows);
    }

    #[test]
    fn side_text_strips_line_endings_and_keeps_offsets() {
        let side = SideText::new("ab\r\ncd\nlast".into());
        let lines: Vec<_> = (0..side.lines.len()).map(|i| side.line(i)).collect();
        assert_eq!(lines, ["ab", "cd", "last"]);
        assert_eq!(side.line_starts, [0, 4, 7]);
        assert_eq!(&side.text[side.byte_range(1, 2)], "cd\n");
        assert_eq!(&side.text[side.byte_range(2, 9)], "last");
    }
}
