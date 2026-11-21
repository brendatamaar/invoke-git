//! Owning tests for PRD laws 1, 2 and 3, plus the oracle and word-diff checks.

use diff::{Algorithm, DiffOptions, LineKind, TextDiff, Whitespace, diff_text, word_diff};
use proptest::prelude::*;

/// Lines drawn from a small pool so repeats are common, with CRLF, blanks
/// and unicode mixed in.
const POOL: [&str; 12] = [
    "",
    "a",
    "b",
    "}",
    "{",
    "  x",
    "\tx",
    "fn main() {",
    "é日本",
    "same",
    "same",
    "𝄞 clef",
];

#[derive(Debug, Clone)]
struct Text {
    lines: Vec<(usize, bool)>, // pool index, CRLF
    final_newline: bool,
}

impl Text {
    fn render(&self) -> String {
        let mut out = String::new();
        for (i, (pool, crlf)) in self.lines.iter().enumerate() {
            out.push_str(POOL[*pool]);
            let last = i + 1 == self.lines.len();
            if !last || self.final_newline {
                out.push_str(if *crlf { "\r\n" } else { "\n" });
            }
        }
        out
    }
}

fn text() -> impl Strategy<Value = Text> {
    (
        prop::collection::vec((0..POOL.len(), prop::bool::weighted(0.3)), 0..40),
        any::<bool>(),
    )
        .prop_map(|(lines, final_newline)| Text {
            lines,
            final_newline,
        })
}

/// A text and an edited copy of it, so most lines are shared.
fn edited_pair() -> impl Strategy<Value = (String, String)> {
    (
        text(),
        prop::collection::vec((any::<prop::sample::Index>(), 0..3u8, 0..POOL.len()), 0..8),
        any::<bool>(),
    )
        .prop_map(|(a, edits, flip_final)| {
            let mut b = a.clone();
            for (at, kind, pool) in edits {
                let len = b.lines.len();
                match kind {
                    0 => b
                        .lines
                        .insert(if len == 0 { 0 } else { at.index(len + 1) }, (pool, false)),
                    1 if len > 0 => {
                        b.lines.remove(at.index(len));
                    }
                    _ if len > 0 => b.lines[at.index(len)] = (pool, pool % 2 == 0),
                    _ => {}
                }
            }
            b.final_newline ^= flip_final;
            (a.render(), b.render())
        })
}

fn any_pair() -> impl Strategy<Value = (String, String)> {
    prop_oneof![
        edited_pair(),
        (text(), text()).prop_map(|(a, b)| (a.render(), b.render())),
    ]
}

fn options(algorithm: Algorithm, context: u32, indent_heuristic: bool) -> DiffOptions {
    DiffOptions {
        algorithm,
        context,
        indent_heuristic,
        ..DiffOptions::default()
    }
}

fn algorithm() -> impl Strategy<Value = Algorithm> {
    prop_oneof![Just(Algorithm::Histogram), Just(Algorithm::Myers)]
}

fn lines_of(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Law 3: every number in every hunk points at a real line, on the right
/// side, with the right content, and headers agree with their lines.
fn check_hunks(diff: &TextDiff, a: &str, b: &str) -> Result<(), String> {
    let (al, bl) = (lines_of(a), lines_of(b));
    let (mut old_end, mut new_end) = (0usize, 0usize);
    let (mut added, mut removed) = (0, 0);
    for (h, hunk) in diff.hunks.iter().enumerate() {
        let first_old = if hunk.old_len == 0 {
            hunk.old_start as usize
        } else {
            hunk.old_start as usize - 1
        };
        let first_new = if hunk.new_len == 0 {
            hunk.new_start as usize
        } else {
            hunk.new_start as usize - 1
        };
        if first_old < old_end || first_new < new_end {
            return Err(format!("hunk {h} overlaps the previous one"));
        }
        if first_old - old_end != first_new - new_end {
            return Err(format!("hunk {h}: skipped lines differ between sides"));
        }
        let (mut next_old, mut next_new) = (first_old, first_new);
        let (mut old_len, mut new_len) = (0u32, 0u32);
        if hunk.lines.iter().all(|l| l.kind == LineKind::Context) {
            return Err(format!("hunk {h} has no change"));
        }
        for line in &hunk.lines {
            let text = format!("{}{}", diff.line_text(line), line.eol.as_str());
            if line.old_no.is_some() != (line.kind != LineKind::Added)
                || line.new_no.is_some() != (line.kind != LineKind::Removed)
            {
                return Err(format!("hunk {h}: wrong line numbers for {:?}", line.kind));
            }
            if let Some(no) = line.old_no {
                if no as usize != next_old + 1 {
                    return Err(format!("hunk {h}: old line {no} out of sequence"));
                }
                if al.get(next_old) != Some(&text.as_str()) {
                    return Err(format!("hunk {h}: old line {no} has other content"));
                }
                next_old += 1;
                old_len += 1;
            }
            if let Some(no) = line.new_no {
                if no as usize != next_new + 1 {
                    return Err(format!("hunk {h}: new line {no} out of sequence"));
                }
                if bl.get(next_new) != Some(&text.as_str()) {
                    return Err(format!("hunk {h}: new line {no} has other content"));
                }
                next_new += 1;
                new_len += 1;
            }
            match line.kind {
                LineKind::Added => added += 1,
                LineKind::Removed => removed += 1,
                LineKind::Context => {}
            }
        }
        if (old_len, new_len) != (hunk.old_len, hunk.new_len) {
            return Err(format!("hunk {h}: header lengths do not match its lines"));
        }
        old_end = next_old;
        new_end = next_new;
    }
    if al.len() - old_end != bl.len() - new_end {
        return Err("lines after the last hunk differ in count".into());
    }
    if (diff.stats.added, diff.stats.removed) != (added, removed) {
        return Err("stats do not match the hunks".into());
    }
    Ok(())
}

/// Length of the longest common subsequence of whole lines.
fn lcs_len(a: &[&str], b: &[&str]) -> usize {
    let mut row = vec![0usize; b.len() + 1];
    for x in a {
        let mut diag = 0;
        for (j, y) in b.iter().enumerate() {
            let up = row[j + 1];
            row[j + 1] = if x == y { diag + 1 } else { up.max(row[j]) };
            diag = up;
        }
    }
    row[b.len()]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Law 1.
    #[test]
    fn law_1_apply_diff_gives_new_text(
        (a, b) in any_pair(),
        algo in algorithm(),
        context in 0..6u32,
        indent in any::<bool>(),
    ) {
        let diff = diff_text(a.as_str(), b.as_str(), &options(algo, context, indent));
        prop_assert_eq!(diff.apply(&a), Ok(b));
    }

    /// Law 2.
    #[test]
    fn law_2_diff_of_same_text_is_empty(
        a in text(),
        algo in algorithm(),
        ws in prop_oneof![
            Just(Whitespace::Exact),
            Just(Whitespace::IgnoreChange),
            Just(Whitespace::IgnoreAll),
        ],
        blank in any::<bool>(),
    ) {
        let a = a.render();
        let opts = DiffOptions {
            algorithm: algo,
            whitespace: ws,
            ignore_blank_lines: blank,
            ..DiffOptions::default()
        };
        let diff = diff_text(a.as_str(), a.as_str(), &opts);
        prop_assert!(diff.is_empty());
        prop_assert_eq!(diff.stats.added + diff.stats.removed, 0);
        prop_assert_eq!(diff.note, None);
    }

    /// Law 3.
    #[test]
    fn law_3_hunk_numbers_map_to_real_lines(
        (a, b) in any_pair(),
        algo in algorithm(),
        context in 0..6u32,
        indent in any::<bool>(),
    ) {
        let diff = diff_text(a.as_str(), b.as_str(), &options(algo, context, indent));
        prop_assert_eq!(check_hunks(&diff, &a, &b), Ok(()));
        // Widening the context later is as valid as asking for it up front.
        let mut wider = diff.clone();
        wider.hunks = diff.hunks_with_context(context + 2);
        prop_assert_eq!(check_hunks(&wider, &a, &b), Ok(()));
    }

    /// Myers is the oracle: it finds a shortest edit script.
    #[test]
    fn myers_edit_count_is_minimal((a, b) in any_pair(), indent in any::<bool>()) {
        let diff = diff_text(a.as_str(), b.as_str(), &options(Algorithm::Myers, 3, indent));
        let (al, bl) = (lines_of(&a), lines_of(&b));
        let minimal = al.len() + bl.len() - 2 * lcs_len(&al, &bl);
        prop_assert_eq!((diff.stats.added + diff.stats.removed) as usize, minimal);
    }

    /// Histogram may be larger than minimal, never smaller.
    #[test]
    fn histogram_is_a_valid_edit_script((a, b) in any_pair()) {
        let diff = diff_text(a.as_str(), b.as_str(), &options(Algorithm::Histogram, 3, true));
        let (al, bl) = (lines_of(&a), lines_of(&b));
        let minimal = al.len() + bl.len() - 2 * lcs_len(&al, &bl);
        prop_assert!((diff.stats.added + diff.stats.removed) as usize >= minimal);
    }

    /// When the budget runs out the answer is still a correct whole replace.
    #[test]
    fn out_of_budget_is_still_correct((a, b) in any_pair(), algo in algorithm()) {
        let opts = DiffOptions { algorithm: algo, max_work: 3, ..DiffOptions::default() };
        let diff = diff_text(a.as_str(), b.as_str(), &opts);
        prop_assert_eq!(diff.apply(&a), Ok(b.clone()));
        prop_assert_eq!(check_hunks(&diff, &a, &b), Ok(()));
    }

    /// Word spans are valid ranges, and apart from whitespace the text left
    /// outside the spans is the same on both sides.
    #[test]
    fn word_spans_leave_the_same_text_outside(
        old in "[a-c_ .é日(]{0,24}",
        new in "[a-c_ .é日(]{0,24}",
    ) {
        if let Some((os, ns)) = word_diff(&old, &new) {
            let outside = |text: &str, spans: &diff::InlineSpans| -> Result<String, String> {
                let mut out = String::new();
                let mut at = 0usize;
                for span in &spans.0 {
                    let (s, e) = (span.start as usize, span.end as usize);
                    if s < at || e <= s || e > text.len()
                        || !text.is_char_boundary(s) || !text.is_char_boundary(e)
                    {
                        return Err(format!("bad span {s}..{e} after {at} in {text:?}"));
                    }
                    out.push_str(&text[at..s]);
                    at = e;
                }
                out.push_str(&text[at..]);
                Ok(out.chars().filter(|c| !c.is_whitespace()).collect())
            };
            let (left, right) = (outside(&old, &os), outside(&new, &ns));
            prop_assert!(left.is_ok() && right.is_ok(), "{:?} {:?}", left, right);
            prop_assert_eq!(left, right);
        }
    }
}
