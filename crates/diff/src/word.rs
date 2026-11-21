//! Word-level diff inside changed line pairs.

use std::collections::HashMap;

use crate::model::{InlineSpans, LineKind, TextDiff, TextRange};
use crate::myers::{Ctx, myers};

/// Pairs sharing less than this fraction of the longer line are not shown
/// with inline highlights.
const MIN_SHARED: f64 = 0.30;
/// Lines with more tokens than this get whole-line colouring only.
const MAX_TOKENS: usize = 4000;
const TOKEN_WORK_LIMIT: u64 = 2_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// Words (identifier characters), runs of whitespace, and single other
/// characters, as byte ranges.
fn tokenize(line: &str) -> Vec<(usize, usize)> {
    let mut tokens = Vec::new();
    let mut chars = line.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let kind = class(c);
        let mut end = start + c.len_utf8();
        if kind != Class::Other {
            while let Some(&(next, d)) = chars.peek() {
                if class(d) != kind {
                    break;
                }
                end = next + d.len_utf8();
                chars.next();
            }
        }
        tokens.push((start, end));
    }
    tokens
}

/// The changed byte ranges on each side of a line pair, or `None` when the
/// lines are too different (or too long) for highlights to help. Ranges are
/// relative to the start of each line.
pub fn word_diff(old: &str, new: &str) -> Option<(InlineSpans, InlineSpans)> {
    let old_tokens = tokenize(old);
    let new_tokens = tokenize(new);
    if old_tokens.len() > MAX_TOKENS || new_tokens.len() > MAX_TOKENS {
        return None;
    }
    let mut table: HashMap<&str, u32> = HashMap::new();
    let a = intern(&mut table, old, &old_tokens);
    let b = intern(&mut table, new, &new_tokens);
    let mut ca = vec![false; a.len()];
    let mut cb = vec![false; b.len()];
    myers(&a, &b, &mut ca, &mut cb, &mut Ctx::new(TOKEN_WORK_LIMIT)).ok()?;

    let shared: usize = old_tokens
        .iter()
        .zip(&ca)
        .filter(|(_, changed)| !**changed)
        .map(|(&(s, e), _)| e - s)
        .sum();
    let longer = old.len().max(new.len());
    if longer > 0 && (shared as f64) < MIN_SHARED * longer as f64 {
        return None;
    }
    Some((spans(old, &old_tokens, &ca), spans(new, &new_tokens, &cb)))
}

fn intern<'a>(
    table: &mut HashMap<&'a str, u32>,
    text: &'a str,
    tokens: &[(usize, usize)],
) -> Vec<u32> {
    tokens
        .iter()
        .map(|&(s, e)| {
            let next = table.len() as u32;
            *table.entry(&text[s..e]).or_insert(next)
        })
        .collect()
}

/// Changed tokens as byte ranges. Runs of changed tokens merge into one span,
/// and two spans split by a single whitespace token merge too, so
/// `foo bar` -> `baz qux` reads as one change.
fn spans(text: &str, tokens: &[(usize, usize)], changed: &[bool]) -> InlineSpans {
    // Runs as (first token, one past the last token).
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if !changed[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < tokens.len() && changed[i] {
            i += 1;
        }
        match runs.last_mut() {
            Some(last)
                if start - last.1 == 1 && {
                    let (s, e) = tokens[last.1];
                    text[s..e].chars().all(char::is_whitespace)
                } =>
            {
                last.1 = i;
            }
            _ => runs.push((start, i)),
        }
    }
    InlineSpans(
        runs.into_iter()
            .map(|(first, end)| TextRange {
                start: tokens[first].0 as u32,
                end: tokens[end - 1].1 as u32,
            })
            .collect(),
    )
}

impl TextDiff {
    /// Computes word-level spans for every removed/added block in one hunk.
    /// Removed and added lines are paired by position inside each block.
    /// Does nothing if the hunk was already filled or does not exist.
    pub fn fill_inline(&mut self, hunk: usize) {
        let old = self.old.clone();
        let new = self.new.clone();
        let Some(hunk) = self.hunks.get_mut(hunk) else {
            return;
        };
        if hunk.inline_filled {
            return;
        }
        hunk.inline_filled = true;
        let lines = &mut hunk.lines;
        let mut i = 0;
        while i < lines.len() {
            if lines[i].kind != LineKind::Removed {
                i += 1;
                continue;
            }
            let removed_start = i;
            while i < lines.len() && lines[i].kind == LineKind::Removed {
                i += 1;
            }
            let added_start = i;
            while i < lines.len() && lines[i].kind == LineKind::Added {
                i += 1;
            }
            let pairs = (added_start - removed_start).min(i - added_start);
            for p in 0..pairs {
                let (r, a) = (removed_start + p, added_start + p);
                let old_text = &old[lines[r].text.as_range()];
                let new_text = &new[lines[a].text.as_range()];
                if let Some((old_spans, new_spans)) = word_diff(old_text, new_text) {
                    lines[r].inline = Some(old_spans);
                    lines[a].inline = Some(new_spans);
                }
            }
        }
    }
}
