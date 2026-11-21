//! Syntax highlighting layer on tree-sitter (backlog task 9).
//!
//! Pure: file text and path/attributes go in as data, styled byte ranges come
//! out. No filesystem, clock, threads or randomness. The caller parses on a
//! background thread and passes a `cancel` closure (it can read a clock or an
//! atomic flag there) to stop a parse or query that runs too long.
//!
//! Pipeline, following `docs/spec/diff-engine.md` "Highlighting pipeline":
//!
//! 1. [`detect`] picks the grammar: `linguist-language` attribute, then file
//!    extension, then shebang. No match is plain text (`None`).
//! 2. [`highlight`] parses the whole file and runs the grammar's highlight
//!    query. Captures are flattened into sorted, non-overlapping [`Span`]s.
//! 3. [`Capture::token`] maps a capture to a design token colour. The caller
//!    styles only the visible window and asks [`spans_in`] for it.

mod detect;
mod flatten;
mod grammar;
mod theme;

pub use detect::{Language, detect};
pub use theme::Capture;

use std::ops::{ControlFlow, Range};

use tree_sitter::{ParseOptions, Parser, QueryCursor, QueryCursorOptions, StreamingIterator};

/// Files over this many bytes are not highlighted (spec "Too large").
pub const MAX_BYTES: usize = 5 * 1024 * 1024;
/// Files over this many lines are not highlighted (spec "Too large").
pub const MAX_LINES: usize = 100_000;

/// A styled byte range. `start..end` is non-empty and on char boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub capture: Capture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HighlightError {
    /// Over [`MAX_BYTES`] or [`MAX_LINES`]; render as plain text with a note.
    TooLarge,
    /// The `cancel` closure returned true; render as plain text with a note.
    Cancelled,
    /// The grammar or its query failed to load. A bug, caught by tests.
    Grammar(String),
}

/// Highlight `text` with `language`. Spans come back sorted by `start`, never
/// overlapping, never empty and always inside `text`.
///
/// `cancel` is polled while parsing and querying; return true to stop.
pub fn highlight(
    language: Language,
    text: &str,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<Span>, HighlightError> {
    if text.len() > MAX_BYTES || text.bytes().filter(|&b| b == b'\n').count() >= MAX_LINES {
        return Err(HighlightError::TooLarge);
    }
    let ts_language = grammar::language(language);
    let query = grammar::query(language).map_err(HighlightError::Grammar)?;

    let mut parser = Parser::new();
    parser
        .set_language(&ts_language)
        .map_err(|e| HighlightError::Grammar(e.to_string()))?;
    let mut parse_cancel = |_: &tree_sitter::ParseState| {
        if cancel() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = ParseOptions::new().progress_callback(&mut parse_cancel);
    let bytes = text.as_bytes();
    let tree = parser
        .parse_with_options(
            &mut |offset, _| {
                if offset < bytes.len() {
                    &bytes[offset..]
                } else {
                    &[]
                }
            },
            None,
            Some(options),
        )
        .ok_or(HighlightError::Cancelled)?;

    let names = query.capture_names();
    let mut raw = Vec::new();
    let mut cursor = QueryCursor::new();
    let query_cancelled = std::cell::Cell::new(false);
    {
        let mut query_cancel = |_: &tree_sitter::QueryCursorState| {
            if cancel() {
                // A stopped cursor just ends its iteration, so remember it:
                // a truncated capture list must never pass as complete.
                query_cancelled.set(true);
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = QueryCursorOptions::new().progress_callback(&mut query_cancel);
        let mut captures = cursor.captures_with_options(query, tree.root_node(), bytes, options);
        while let Some((m, index)) = captures.next() {
            let capture = m.captures()[*index];
            let Some(kind) = Capture::from_name(names[capture.index as usize]) else {
                continue;
            };
            let range = capture.node.byte_range();
            raw.push(flatten::Raw {
                start: range.start,
                end: range.end,
                capture: kind,
                pattern: m.pattern_index,
            });
        }
    }
    if query_cancelled.get() {
        return Err(HighlightError::Cancelled);
    }
    Ok(flatten::flatten(text, raw))
}

/// The spans that touch `range` (a byte range, usually the visible lines plus
/// the buffer). `spans` must be sorted and non-overlapping, as [`highlight`]
/// returns it. Spans are not clipped to `range`.
pub fn spans_in(spans: &[Span], range: Range<usize>) -> &[Span] {
    let first = spans.partition_point(|s| s.end <= range.start);
    let last = spans.partition_point(|s| s.start < range.end);
    if first >= last {
        &[]
    } else {
        &spans[first..last]
    }
}
