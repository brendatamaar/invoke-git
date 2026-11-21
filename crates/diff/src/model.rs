//! The diff data model. Lines point into two shared buffers, so a 50k-line
//! diff costs the two file buffers plus small fixed-size line records.

use std::ops::Range;
use std::sync::Arc;

/// Which algorithm computes the line diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Algorithm {
    /// Histogram anchors, Myers inside regions with no anchor.
    #[default]
    Histogram,
    /// Plain Myers, the reference and test oracle.
    Myers,
}

/// How whitespace is treated when comparing lines. Applied before hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Whitespace {
    #[default]
    Exact,
    /// Runs of whitespace are equal, trailing whitespace is ignored.
    IgnoreChange,
    /// All whitespace is ignored.
    IgnoreAll,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOptions {
    pub algorithm: Algorithm,
    pub whitespace: Whitespace,
    /// Drop hunks whose only changes are blank lines.
    pub ignore_blank_lines: bool,
    /// Lines of context around each change. Default 3.
    pub context: u32,
    /// Slide changes to natural block edges.
    pub indent_heuristic: bool,
    /// Work units (roughly one per line compared) before the diff gives up and
    /// reports a whole-file replace. The caller maps its 200 ms budget to this.
    pub max_work: u64,
}

impl Default for DiffOptions {
    fn default() -> Self {
        DiffOptions {
            algorithm: Algorithm::Histogram,
            whitespace: Whitespace::Exact,
            ignore_blank_lines: false,
            context: 3,
            indent_heuristic: true,
            max_work: 50_000_000,
        }
    }
}

/// Why a diff is not the plain minimal answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffNote {
    /// The work budget ran out; the result is one whole-file replace hunk.
    TooExpensive,
    /// A buffer is over 4 GiB; no hunks were produced.
    InputTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

/// How a line ends. `None` is the last line of a file with no final newline;
/// it is part of the model because staging must preserve it exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    CrLf,
    None,
}

impl Eol {
    pub fn as_str(self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::CrLf => "\r\n",
            Eol::None => "",
        }
    }
}

/// Byte range into a file buffer, excluding the line ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub start: u32,
    pub end: u32,
}

impl TextRange {
    pub fn as_range(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }
}

/// Word-level changes on one line, as byte ranges relative to the start of
/// the line's text.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InlineSpans(pub Vec<TextRange>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// 1-based line number in the old file; `None` for added lines.
    pub old_no: Option<u32>,
    /// 1-based line number in the new file; `None` for removed lines.
    pub new_no: Option<u32>,
    /// Into the old buffer for `Removed`, the new buffer otherwise.
    pub text: TextRange,
    pub eol: Eol,
    /// Filled lazily by [`TextDiff::fill_inline`].
    pub inline: Option<InlineSpans>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based. When `old_len` is 0 this is the line before the hunk (0 at the
    /// top of the file), as in git's unified format.
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// Nearest enclosing function; filled by the highlighting layer (task 9).
    pub header: Option<String>,
    pub lines: Vec<DiffLine>,
    /// True once word-level spans were computed for this hunk.
    pub inline_filled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub added: u32,
    pub removed: u32,
}

/// One maximal changed region: old lines `a0..a1` were replaced by new lines
/// `b0..b1` (0-based, half open). Kept so context can be widened without
/// recomputing the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub a0: usize,
    pub a1: usize,
    pub b0: usize,
    pub b1: usize,
}

/// The diff of one text against another, with both buffers.
#[derive(Debug, Clone)]
pub struct TextDiff {
    pub old: Arc<str>,
    pub new: Arc<str>,
    pub hunks: Vec<Hunk>,
    pub stats: Stats,
    pub note: Option<DiffNote>,
    pub(crate) changes: Vec<Change>,
    pub(crate) old_starts: Vec<u32>,
    pub(crate) new_starts: Vec<u32>,
    pub(crate) ignore_blank_lines: bool,
}

impl TextDiff {
    /// True when the two texts are identical (no hunks).
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }

    /// The text of a line (without its ending), from the right buffer.
    pub fn line_text(&self, line: &DiffLine) -> &str {
        let buffer = match line.kind {
            LineKind::Removed => &self.old,
            _ => &self.new,
        };
        &buffer[line.text.as_range()]
    }
}
