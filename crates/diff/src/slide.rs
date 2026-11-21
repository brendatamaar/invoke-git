//! Post-processing: slide each changed group to a natural block edge.
//!
//! A group of changed lines can often move up or down while the diff stays
//! equivalent, because the line leaving one end equals the line entering the
//! other. Moving one step swaps an unchanged line for an equal one at the same
//! rank among the unchanged lines, so both sides still pair up. This is a
//! simplified form of git's indent heuristic.

use crate::lines::{full_line, line_count};

/// How far a group may move in either direction.
const MAX_SLIDE: usize = 100;

/// Indentation of each line; `None` for blank lines.
pub(crate) fn indents(text: &str, starts: &[u32]) -> Vec<Option<u32>> {
    (0..line_count(starts))
        .map(|i| {
            let line = full_line(text, starts, i);
            if line.trim().is_empty() {
                return None;
            }
            let mut width = 0;
            for c in line.chars() {
                match c {
                    ' ' => width += 1,
                    '\t' => width += 4,
                    _ => break,
                }
            }
            Some(width)
        })
        .collect()
}

/// Lower is better.
fn score(indent: &[Option<u32>], start: usize, end: usize) -> i32 {
    let mut score = 0;
    // Starting right after a blank line (or the top) reads as a new block.
    if start == 0 || indent[start - 1].is_none() {
        score -= 2;
    }
    // Ending on a blank line, or right before one, closes a block.
    if indent[end - 1].is_none() {
        score -= 1;
    }
    if end == indent.len() || indent[end].is_none() {
        score -= 1;
    }
    // Ending just before a deeper line cuts a block in half.
    if let (Some(last), Some(Some(next))) = (indent[end - 1], indent.get(end))
        && *next > last
    {
        score += 2;
    }
    score
}

/// Slides every group of `changed` lines to its best position. `ids` are the
/// interned lines of this side, used to decide whether a step is allowed.
pub(crate) fn slide(ids: &[u32], indent: &[Option<u32>], changed: &mut [bool]) {
    let n = ids.len();
    let mut i = 0;
    while i < n {
        if !changed[i] {
            i += 1;
            continue;
        }
        let mut start = i;
        let mut end = i;
        while end < n && changed[end] {
            end += 1;
        }
        // Up as far as it goes.
        let mut up = 0;
        while up < MAX_SLIDE && start > 0 && !changed[start - 1] && ids[start - 1] == ids[end - 1] {
            changed[start - 1] = true;
            changed[end - 1] = false;
            start -= 1;
            end -= 1;
            up += 1;
        }
        // Count the steps back down that stay legal.
        let mut down = 0;
        while down < 2 * MAX_SLIDE
            && end + down < n
            && !changed[end + down]
            && ids[start + down] == ids[end + down]
        {
            down += 1;
        }
        // Pick the best position; later wins ties, as git prefers sliding down.
        let mut best = 0;
        let mut best_score = i32::MAX;
        for shift in 0..=down {
            let s = score(indent, start + shift, end + shift);
            if s <= best_score {
                best_score = s;
                best = shift;
            }
        }
        if best != 0 {
            for line in &mut changed[start..end] {
                *line = false;
            }
            for line in &mut changed[start + best..end + best] {
                *line = true;
            }
        }
        i = end + best;
    }
}
