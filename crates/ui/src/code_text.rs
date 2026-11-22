//! Pure helpers for drawing one line of code: tabs, and the merge of syntax
//! colours with word-level change marks into non-overlapping styled runs.
//! No window is needed, so everything here is unit and property tested.

use std::ops::Range;

use highlight::Capture;

/// Display width of a tab, in spaces. Fixed: a tab is not aligned to a tab
/// stop, because a diff line is drawn without its neighbours.
pub const TAB_WIDTH: usize = 4;

/// The line as drawn: every tab becomes [`TAB_WIDTH`] spaces.
pub fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_owned();
    }
    text.replace('\t', &" ".repeat(TAB_WIDTH))
}

/// Where byte `offset` of `text` lands in [`expand_tabs`]`(text)`. `offset`
/// must be at most `text.len()`.
pub fn expanded_offset(text: &str, offset: usize) -> usize {
    let tabs = text.as_bytes()[..offset]
        .iter()
        .filter(|byte| **byte == b'\t')
        .count();
    offset + tabs * (TAB_WIDTH - 1)
}

/// How a run of a line is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    /// Syntax colour, if the run is a keyword, string, and so on.
    pub capture: Option<Capture>,
    /// The run is part of a word-level change (a stronger background).
    pub changed: bool,
}

/// Merges two sorted, non-overlapping lists of byte ranges into runs that
/// carry both. Runs that are neither coloured nor changed are left out, so
/// plain text produces nothing. Ranges are clipped to `0..len`; empty ranges
/// are ignored. The result is sorted and non-overlapping.
pub fn merge_styles(
    len: usize,
    syntax: &[(Range<usize>, Capture)],
    changed: &[Range<usize>],
) -> Vec<(Range<usize>, Style)> {
    let mut points: Vec<usize> = vec![0, len];
    for (range, _) in syntax {
        points.push(range.start.min(len));
        points.push(range.end.min(len));
    }
    for range in changed {
        points.push(range.start.min(len));
        points.push(range.end.min(len));
    }
    points.sort_unstable();
    points.dedup();

    let (mut s, mut c) = (0, 0);
    let mut runs: Vec<(Range<usize>, Style)> = Vec::new();
    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        while s < syntax.len() && syntax[s].0.end.min(len) <= a {
            s += 1;
        }
        while c < changed.len() && changed[c].end.min(len) <= a {
            c += 1;
        }
        let capture = syntax
            .get(s)
            .filter(|(range, _)| range.start <= a && b <= range.end.min(len))
            .map(|(_, capture)| *capture);
        let is_changed = changed
            .get(c)
            .is_some_and(|range| range.start <= a && b <= range.end.min(len));
        if capture.is_none() && !is_changed {
            continue;
        }
        let style = Style {
            capture,
            changed: is_changed,
        };
        // Neighbouring runs with one style are one run.
        match runs.last_mut() {
            Some((last, last_style)) if last.end == a && *last_style == style => last.end = b,
            _ => runs.push((a..b, style)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn tabs_become_four_spaces() {
        assert_eq!(expand_tabs("\tfn a()"), "    fn a()");
        assert_eq!(expand_tabs("no tabs"), "no tabs");
    }

    #[test]
    fn offsets_move_with_the_tabs_before_them() {
        let text = "\t\tab\tc";
        let drawn = expand_tabs(text);
        // `a` is at byte 2 of the text and must still be `a` when drawn.
        assert_eq!(&drawn[expanded_offset(text, 2)..][..1], "a");
        // `c` is at byte 5.
        assert_eq!(&drawn[expanded_offset(text, 5)..][..1], "c");
        assert_eq!(expanded_offset(text, text.len()), drawn.len());
    }

    #[test]
    fn colours_and_changes_overlap_into_runs() {
        let runs = merge_styles(
            10,
            &[(0..4, Capture::Keyword)],
            &[Range { start: 2, end: 6 }],
        );
        assert_eq!(
            runs,
            vec![
                (
                    0..2,
                    Style {
                        capture: Some(Capture::Keyword),
                        changed: false
                    }
                ),
                (
                    2..4,
                    Style {
                        capture: Some(Capture::Keyword),
                        changed: true
                    }
                ),
                (
                    4..6,
                    Style {
                        capture: None,
                        changed: true
                    }
                ),
            ]
        );
    }

    #[test]
    fn plain_text_has_no_runs() {
        assert!(merge_styles(5, &[], &[]).is_empty());
        assert!(
            merge_styles(0, &[(0..3, Capture::String)], &[Range { start: 0, end: 3 }]).is_empty()
        );
    }

    /// Sorted, non-overlapping ranges inside `0..len`.
    fn ranges(len: usize) -> impl Strategy<Value = Vec<Range<usize>>> {
        proptest::collection::vec(0..=len, 0..8).prop_map(|mut points| {
            points.sort_unstable();
            points.dedup();
            points
                .chunks(2)
                .filter(|pair| pair.len() == 2)
                .map(|pair| pair[0]..pair[1])
                .collect()
        })
    }

    proptest! {
        /// Byte by byte, a run's style is the style of every byte in it,
        /// and bytes in no run have neither a colour nor a change mark.
        #[test]
        fn merged_runs_equal_the_bytewise_answer(
            len in 0usize..60,
            seed in any::<u64>(),
        ) {
            let mut rng = seed;
            let mut next = |bound: usize| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (rng >> 33) as usize % bound.max(1)
            };
            // Build sorted, non-overlapping inputs from the seed.
            let mut syntax = Vec::new();
            let mut at = 0;
            while at < len {
                let start = at + next(5);
                let end = (start + 1 + next(6)).min(len + 3);
                if start >= len { break; }
                let capture = Capture::ALL[next(Capture::ALL.len())];
                syntax.push((start..end, capture));
                at = end;
            }
            let mut changed = Vec::new();
            let mut at = 0;
            while at < len {
                let start = at + next(5);
                let end = (start + 1 + next(6)).min(len + 3);
                if start >= len { break; }
                changed.push(start..end);
                at = end;
            }
            let runs = merge_styles(len, &syntax, &changed);
            // Sorted, non-overlapping, non-empty, inside the line.
            let mut previous_end = 0;
            for (range, _) in &runs {
                prop_assert!(range.start < range.end && range.end <= len);
                prop_assert!(range.start >= previous_end);
                previous_end = range.end;
            }
            for byte in 0..len {
                let want_capture = syntax
                    .iter()
                    .find(|(r, _)| r.start <= byte && byte < r.end)
                    .map(|(_, c)| *c);
                let want_changed = changed.iter().any(|r| r.start <= byte && byte < r.end);
                let got = runs
                    .iter()
                    .find(|(r, _)| r.start <= byte && byte < r.end)
                    .map(|(_, s)| *s);
                match got {
                    Some(style) => {
                        prop_assert_eq!(style.capture, want_capture);
                        prop_assert_eq!(style.changed, want_changed);
                    }
                    None => {
                        prop_assert!(want_capture.is_none() && !want_changed);
                    }
                }
            }
        }

        /// `ranges` sorted input never makes the merge panic.
        #[test]
        fn merge_never_panics(len in 0usize..40, a in ranges(40), b in ranges(40)) {
            let syntax: Vec<_> = a.into_iter().map(|r| (r, Capture::Keyword)).collect();
            let _ = merge_styles(len, &syntax, &b);
        }

        /// Expanded offsets keep pointing at the same character.
        #[test]
        fn expanded_offsets_keep_their_character(text in "[ab\t]{0,20}") {
            let drawn = expand_tabs(&text);
            for (offset, ch) in text.char_indices() {
                if ch != '\t' {
                    let moved = expanded_offset(&text, offset);
                    prop_assert_eq!(drawn[moved..].chars().next(), Some(ch));
                }
            }
        }
    }
}
