//! Splitting text into lines and interning them to integer ids.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::model::{Eol, Whitespace};

/// Start offsets of every line, plus the end of the text as a last entry, so
/// line `i` is `starts[i]..starts[i + 1]` (ending included).
pub(crate) fn line_starts(text: &str) -> Vec<u32> {
    let mut starts = vec![0u32];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push((index + 1) as u32);
        }
    }
    if *starts.last().unwrap_or(&0) as usize != text.len() {
        starts.push(text.len() as u32);
    }
    // An empty text has no lines: starts == [0].
    starts
}

pub(crate) fn line_count(starts: &[u32]) -> usize {
    starts.len().saturating_sub(1)
}

/// The full line, ending included.
pub(crate) fn full_line<'a>(text: &'a str, starts: &[u32], index: usize) -> &'a str {
    &text[starts[index] as usize..starts[index + 1] as usize]
}

/// Splits a full line into its text range (offsets in the buffer) and ending.
pub(crate) fn split_ending(text: &str, starts: &[u32], index: usize) -> (u32, u32, Eol) {
    let start = starts[index];
    let end = starts[index + 1];
    let line = &text[start as usize..end as usize];
    if line.ends_with("\r\n") {
        (start, end - 2, Eol::CrLf)
    } else if line.ends_with('\n') {
        (start, end - 1, Eol::Lf)
    } else {
        (start, end, Eol::None)
    }
}

fn key(line: &str, whitespace: Whitespace) -> Cow<'_, str> {
    match whitespace {
        Whitespace::Exact => Cow::Borrowed(line),
        Whitespace::IgnoreAll => Cow::Owned(line.chars().filter(|c| !c.is_whitespace()).collect()),
        Whitespace::IgnoreChange => {
            let mut out = String::with_capacity(line.len());
            let mut in_space = false;
            for c in line.trim_end().chars() {
                if c.is_whitespace() {
                    if !in_space {
                        out.push(' ');
                    }
                    in_space = true;
                } else {
                    out.push(c);
                    in_space = false;
                }
            }
            Cow::Owned(out)
        }
    }
}

/// Interns the lines of both texts to ids from one shared table.
pub(crate) fn intern<'a>(
    old: &'a str,
    old_starts: &[u32],
    new: &'a str,
    new_starts: &[u32],
    whitespace: Whitespace,
) -> (Vec<u32>, Vec<u32>) {
    let mut table: HashMap<Cow<'a, str>, u32> = HashMap::new();
    let mut side = |text: &'a str, starts: &[u32]| -> Vec<u32> {
        (0..line_count(starts))
            .map(|i| {
                let next = table.len() as u32;
                *table
                    .entry(key(full_line(text, starts, i), whitespace))
                    .or_insert(next)
            })
            .collect()
    };
    let a = side(old, old_starts);
    let b = side(new, new_starts);
    (a, b)
}
