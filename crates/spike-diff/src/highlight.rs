//! Tree-sitter highlighting, following docs/spec/diff-engine.md: parse the
//! whole file once in the background, then run the highlight query only for
//! chunks of lines around the visible window.

use std::ops::Range;
use std::sync::Arc;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor, Tree};

use crate::model::SideText;

/// Lines per highlight chunk; the unit of caching and background work.
pub const CHUNK: usize = 128;
/// Extra lines styled above and below the visible window.
pub const BUFFER: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Keyword,
    String,
    Comment,
    Type,
    Function,
    Number,
}

/// Spans for one line, as byte ranges within that line, sorted and disjoint.
pub type LineSpans = Vec<(Range<usize>, Token)>;
/// Spans for every line of one chunk.
pub type ChunkSpans = Arc<Vec<LineSpans>>;

pub struct Highlighter {
    query: Query,
    /// Capture index → token, `None` for captures we do not colour.
    tokens: Vec<Option<Token>>,
}

fn token_for(capture: &str) -> Option<Token> {
    match capture.split('.').next()? {
        "keyword" => Some(Token::Keyword),
        "string" | "escape" => Some(Token::String),
        "comment" => Some(Token::Comment),
        "type" | "constructor" => Some(Token::Type),
        "function" => Some(Token::Function),
        // tree-sitter-rust tags integer, float and boolean literals this way.
        "constant" if capture == "constant.builtin" => Some(Token::Number),
        _ => None,
    }
}

impl Highlighter {
    pub fn new() -> Self {
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, tree_sitter_rust::HIGHLIGHTS_QUERY)
            .expect("the bundled Rust highlight query compiles");
        let tokens = query.capture_names().iter().map(|n| token_for(n)).collect();
        Self { query, tokens }
    }

    pub fn parse(text: &str) -> Option<Tree> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("the Rust grammar loads");
        parser.parse(text, None)
    }

    /// Styles lines `chunk * CHUNK ..` of one side.
    pub fn style_chunk(&self, tree: &Tree, side: &SideText, chunk: usize) -> Vec<LineSpans> {
        let first = chunk * CHUNK;
        let last = (first + CHUNK).min(side.lines.len());
        let mut out = vec![LineSpans::new(); last.saturating_sub(first)];
        if out.is_empty() {
            return out;
        }
        let range = side.byte_range(first, last);
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range.clone());
        let mut captures = cursor.captures(&self.query, tree.root_node(), side.text.as_bytes());
        while let Some((found, index)) = captures.next() {
            let capture = found.captures()[*index];
            let Some(token) = self.tokens[capture.index as usize] else {
                continue;
            };
            let bytes = capture.node.byte_range();
            let (start, end) = (bytes.start.max(range.start), bytes.end.min(range.end));
            if start >= end {
                continue;
            }
            // Clip the capture to each line it touches (block comments and
            // multi-line strings span several).
            let mut line =
                first + side.line_starts[first..last].partition_point(|&s| s <= start) - 1;
            while line < last && side.line_starts[line] < end {
                let line_start = side.line_starts[line];
                let line_len = side.lines[line].len();
                let from = start.saturating_sub(line_start);
                let to = (end - line_start).min(line_len);
                if from < to {
                    let spans = &mut out[line - first];
                    // Captures arrive ordered by start; the first one to claim
                    // a byte wins, so spans stay disjoint for GPUI.
                    if spans.last().is_none_or(|(prev, _)| prev.end <= from) {
                        spans.push((from..to, token));
                    }
                }
                line += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(source: &str) -> (SideText, Vec<LineSpans>) {
        let side = SideText::new(source.into());
        let tree = Highlighter::parse(&side.text).unwrap();
        let spans = Highlighter::new().style_chunk(&tree, &side, 0);
        (side, spans)
    }

    fn tokens_on(side: &SideText, spans: &[LineSpans], line: usize) -> Vec<(String, Token)> {
        spans[line]
            .iter()
            .map(|(r, t)| (side.line(line)[r.clone()].to_string(), *t))
            .collect()
    }

    #[test]
    fn maps_captures_to_tokens() {
        let (side, spans) = styled("// note\nfn run(x: u32) -> u32 { let s = \"hi\"; 42 }\n");
        assert_eq!(
            tokens_on(&side, &spans, 0),
            [("// note".into(), Token::Comment)]
        );
        let line = tokens_on(&side, &spans, 1);
        for expected in [
            ("fn", Token::Keyword),
            ("run", Token::Function),
            ("u32", Token::Type),
            ("let", Token::Keyword),
            ("\"hi\"", Token::String),
            ("42", Token::Number),
        ] {
            assert!(
                line.contains(&(expected.0.to_string(), expected.1)),
                "missing {expected:?} in {line:?}"
            );
        }
    }

    #[test]
    fn spans_are_sorted_disjoint_and_inside_their_line() {
        // `Some(2)` is captured twice (@constructor and @function), so the
        // overlap guard is exercised.
        let source = "/* a\n  b */ struct S { f: Vec<u8> }\nimpl S { fn g(&self) { println!(\"{}\", 1.5); Some(2); } }\n";
        let (side, spans) = styled(source);
        assert!(!spans[0].is_empty(), "block comment's first line is styled");
        for (line, spans) in spans.iter().enumerate() {
            let mut end = 0;
            for (range, _) in spans {
                assert!(range.start >= end && range.start < range.end);
                assert!(range.end <= side.lines[line].len());
                end = range.end;
            }
        }
    }

    #[test]
    fn later_chunks_use_their_own_lines() {
        let source: String = (0..CHUNK + 3)
            .map(|i| format!("let v{i} = {i};\n"))
            .collect();
        let side = SideText::new(source);
        let tree = Highlighter::parse(&side.text).unwrap();
        let spans = Highlighter::new().style_chunk(&tree, &side, 1);
        assert_eq!(spans.len(), 3);
        let line = side.line(CHUNK + 2);
        let found: Vec<_> = spans[2]
            .iter()
            .map(|(r, t)| (&line[r.clone()], *t))
            .collect();
        let number = (CHUNK + 2).to_string();
        assert!(found.contains(&("let", Token::Keyword)), "{found:?}");
        assert!(
            found.contains(&(number.as_str(), Token::Number)),
            "{found:?}"
        );
    }
}
