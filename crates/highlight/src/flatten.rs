//! Turn overlapping query captures into sorted, non-overlapping spans.

use crate::{Capture, Span};

/// One capture as the query produced it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Raw {
    pub start: usize,
    pub end: usize,
    pub capture: Capture,
    pub pattern: usize,
}

/// On each stretch of text the innermost (shortest) capture wins; on equal
/// ranges the later query pattern wins. Adjacent stretches with the same
/// capture merge. Ranges are clamped to `text` and snapped outward to char
/// boundaries, and empty results are dropped.
pub(crate) fn flatten(text: &str, raw: Vec<Raw>) -> Vec<Span> {
    let mut items: Vec<Raw> = raw
        .into_iter()
        .filter_map(|mut r| {
            r.start = floor_boundary(text, r.start);
            r.end = ceil_boundary(text, r.end);
            (r.start < r.end).then_some(r)
        })
        .collect();
    items.sort_by_key(|r| (r.start, r.end));

    let mut points: Vec<usize> = items.iter().flat_map(|r| [r.start, r.end]).collect();
    points.sort_unstable();
    points.dedup();

    let mut out: Vec<Span> = Vec::new();
    let mut active: Vec<usize> = Vec::new();
    let mut next = 0;
    for pair in points.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        while next < items.len() && items[next].start <= from {
            active.push(next);
            next += 1;
        }
        active.retain(|&i| items[i].end > from);
        let best = active.iter().copied().min_by_key(|&i| {
            (
                items[i].end - items[i].start,
                std::cmp::Reverse(items[i].pattern),
            )
        });
        let Some(best) = best else { continue };
        let capture = items[best].capture;
        match out.last_mut() {
            Some(last) if last.end == from && last.capture == capture => last.end = to,
            _ => out.push(Span {
                start: from,
                end: to,
                capture,
            }),
        }
    }
    out
}

fn floor_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(text: &str, mut i: usize) -> usize {
    i = i.min(text.len());
    while !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(start: usize, end: usize, capture: Capture, pattern: usize) -> Raw {
        Raw {
            start,
            end,
            capture,
            pattern,
        }
    }

    fn span(start: usize, end: usize, capture: Capture) -> Span {
        Span {
            start,
            end,
            capture,
        }
    }

    #[test]
    fn innermost_capture_wins_and_outer_resumes() {
        let out = flatten(
            "0123456789",
            vec![
                raw(0, 10, Capture::String, 0),
                raw(3, 6, Capture::Number, 1),
            ],
        );
        assert_eq!(
            out,
            vec![
                span(0, 3, Capture::String),
                span(3, 6, Capture::Number),
                span(6, 10, Capture::String),
            ]
        );
    }

    #[test]
    fn later_pattern_wins_on_equal_ranges() {
        let out = flatten(
            "abcd",
            vec![raw(0, 4, Capture::Type, 1), raw(0, 4, Capture::Function, 2)],
        );
        assert_eq!(out, vec![span(0, 4, Capture::Function)]);
    }

    #[test]
    fn adjacent_same_capture_merges_and_gaps_stay() {
        let out = flatten(
            "abcdef",
            vec![
                raw(0, 2, Capture::Keyword, 0),
                raw(2, 3, Capture::Keyword, 0),
                raw(4, 6, Capture::Keyword, 0),
            ],
        );
        assert_eq!(
            out,
            vec![span(0, 3, Capture::Keyword), span(4, 6, Capture::Keyword)]
        );
    }

    #[test]
    fn ranges_snap_outward_to_char_boundaries() {
        // "é" is bytes 0..2; a range 1..3 snaps to 0..3.
        let out = flatten(
            "éa",
            vec![
                raw(1, 3, Capture::Comment, 0),
                raw(9, 12, Capture::Comment, 0),
            ],
        );
        assert_eq!(out, vec![span(0, 3, Capture::Comment)]);
        // "é" is bytes 1..3 here; a range 0..2 ends inside it and grows to 0..3.
        let out = flatten("aé", vec![raw(0, 2, Capture::Comment, 0)]);
        assert_eq!(out, vec![span(0, 3, Capture::Comment)]);
    }

    #[test]
    fn input_order_does_not_matter() {
        let out = flatten(
            "abcdef",
            vec![
                raw(4, 6, Capture::Number, 2),
                raw(0, 6, Capture::String, 0),
                raw(2, 4, Capture::Type, 1),
            ],
        );
        assert_eq!(
            out,
            vec![
                span(0, 2, Capture::String),
                span(2, 4, Capture::Type),
                span(4, 6, Capture::Number),
            ]
        );
    }
}
