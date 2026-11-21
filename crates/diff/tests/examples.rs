//! Worked examples, edge cases and the speed sanity check.

use diff::{
    Algorithm, ApplyError, DiffNote, DiffOptions, Eol, LineKind, Whitespace, diff_text, word_diff,
};

fn opts() -> DiffOptions {
    DiffOptions::default()
}

fn kinds(diff: &diff::TextDiff, hunk: usize) -> String {
    diff.hunks[hunk]
        .lines
        .iter()
        .map(|l| match l.kind {
            LineKind::Context => ' ',
            LineKind::Added => '+',
            LineKind::Removed => '-',
        })
        .collect()
}

#[test]
fn replace_in_the_middle_has_git_style_header() {
    let a = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
    let b = "1\n2\n3\n4\nfive\n6\n7\n8\n9\n";
    let d = diff_text(a, b, &opts());
    assert_eq!(d.hunks.len(), 1);
    let h = &d.hunks[0];
    assert_eq!(
        (h.old_start, h.old_len, h.new_start, h.new_len),
        (2, 7, 2, 7)
    );
    assert_eq!(kinds(&d, 0), "   -+   ");
    assert_eq!((d.stats.added, d.stats.removed), (1, 1));
}

#[test]
fn pure_insert_at_top_uses_start_zero() {
    let d = diff_text(
        "a\n",
        "new\na\n",
        &DiffOptions {
            context: 0,
            ..opts()
        },
    );
    let h = &d.hunks[0];
    assert_eq!(
        (h.old_start, h.old_len, h.new_start, h.new_len),
        (0, 0, 1, 1)
    );
}

#[test]
fn distant_changes_make_separate_hunks_and_close_ones_merge() {
    let a: String = (1..=30).map(|i| format!("{i}\n")).collect();
    let b = a
        .replace("\n5\n", "\nfive\n")
        .replace("\n25\n", "\ntwentyfive\n");
    assert_eq!(diff_text(a.as_str(), b.as_str(), &opts()).hunks.len(), 2);
    let d = diff_text(a.as_str(), b.as_str(), &opts());
    assert_eq!(d.hunks_with_context(30).len(), 1);
    let c = a.replace("\n5\n", "\nfive\n").replace("\n9\n", "\nnine\n");
    assert_eq!(diff_text(a.as_str(), c.as_str(), &opts()).hunks.len(), 1);
}

#[test]
fn hunks_merge_exactly_when_their_context_touches() {
    let a: String = (1..=30).map(|i| format!("{i}\n")).collect();
    // Six unchanged lines between the changes: 3 + 3 of context meet.
    let six = a.replace("\n5\n", "\nx\n").replace("\n12\n", "\ny\n");
    assert_eq!(diff_text(a.as_str(), six.as_str(), &opts()).hunks.len(), 1);
    // Seven lines between: one line of the file is in no hunk.
    let seven = a.replace("\n5\n", "\nx\n").replace("\n13\n", "\ny\n");
    assert_eq!(
        diff_text(a.as_str(), seven.as_str(), &opts()).hunks.len(),
        2
    );
}

#[test]
fn missing_final_newline_is_part_of_the_model() {
    let d = diff_text("a\nb", "a\nb\n", &opts());
    assert_eq!(d.hunks.len(), 1);
    let lines = &d.hunks[0].lines;
    let removed = lines.iter().find(|l| l.kind == LineKind::Removed).unwrap();
    let added = lines.iter().find(|l| l.kind == LineKind::Added).unwrap();
    assert_eq!((removed.eol, added.eol), (Eol::None, Eol::Lf));
    assert_eq!(d.line_text(removed), "b");
    assert_eq!(d.apply("a\nb").unwrap(), "a\nb\n");
}

#[test]
fn crlf_and_lf_lines_differ() {
    let d = diff_text("a\r\nb\r\n", "a\nb\n", &opts());
    assert_eq!((d.stats.added, d.stats.removed), (2, 2));
    let removed = &d.hunks[0].lines[0];
    assert_eq!(removed.eol, Eol::CrLf);
    assert_eq!(d.line_text(removed), "a");
    // A lone CR is not a line ending.
    let d = diff_text("a\rb\n", "a\rc\n", &opts());
    assert_eq!(d.stats.removed, 1);
    assert_eq!(d.line_text(&d.hunks[0].lines[0]), "a\rb");
}

#[test]
fn empty_texts() {
    assert!(diff_text("", "", &opts()).is_empty());
    let d = diff_text("", "x\n", &opts());
    assert_eq!(kinds(&d, 0), "+");
    assert_eq!(d.apply("").unwrap(), "x\n");
    let d = diff_text("x\n", "", &opts());
    assert_eq!(kinds(&d, 0), "-");
    assert_eq!(d.apply("x\n").unwrap(), "");
    // "" has no lines, "\n" has one empty line.
    assert_eq!(diff_text("", "\n", &opts()).stats.added, 1);
}

#[test]
fn apply_refuses_the_wrong_base() {
    let d = diff_text("a\nb\nc\n", "a\nX\nc\n", &opts());
    assert!(matches!(
        d.apply("a\nQ\nc\n"),
        Err(ApplyError::Mismatch { .. })
    ));
    assert!(d.apply("").is_err());
}

#[test]
fn whitespace_options() {
    let a = "fn  a() {\n    x\n}\n";
    let b = "fn a() {\n  x  \n}\n";
    assert!(!diff_text(a, b, &opts()).is_empty());
    for ws in [Whitespace::IgnoreChange, Whitespace::IgnoreAll] {
        let d = diff_text(
            a,
            b,
            &DiffOptions {
                whitespace: ws,
                ..opts()
            },
        );
        assert!(d.is_empty(), "{ws:?}");
    }
    let c = "fn a() {\n  x\n}\n";
    let d = diff_text(
        c,
        "fn a() {\n  x\n\n}\n",
        &DiffOptions {
            ignore_blank_lines: true,
            ..opts()
        },
    );
    assert!(d.is_empty());
}

#[test]
fn histogram_anchors_on_the_rare_line() {
    // `}` is everywhere; `unique` is the anchor, so the moved block is
    // reported as one insert and one delete rather than a smeared edit.
    let a = "}\n}\nunique\n}\n}\n";
    let b = "}\n}\n}\nunique\n}\n}\n";
    for algo in [Algorithm::Histogram, Algorithm::Myers] {
        let d = diff_text(
            a,
            b,
            &DiffOptions {
                algorithm: algo,
                ..opts()
            },
        );
        assert_eq!(d.stats.added + d.stats.removed, 1, "{algo:?}");
    }
}

#[test]
fn changes_slide_to_block_edges() {
    let a = "fn a() {\n}\n\nfn c() {\n}\n";
    let b = "fn a() {\n}\n\nfn b() {\n}\n\nfn c() {\n}\n";
    let d = diff_text(a, b, &opts());
    let added: Vec<_> = d.hunks[0]
        .lines
        .iter()
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| (l.new_no.unwrap(), d.line_text(l).to_string()))
        .collect();
    // The new function and its trailing blank line, starting after a blank.
    assert_eq!(
        added,
        vec![(4, "fn b() {".into()), (5, "}".into()), (6, "".into())]
    );
}

#[test]
fn too_expensive_becomes_a_whole_replace() {
    let a: String = (0..200).map(|i| format!("{}\n", i * 7 % 13)).collect();
    let b: String = (0..200).map(|i| format!("{}\n", i * 5 % 11)).collect();
    let d = diff_text(
        a.as_str(),
        b.as_str(),
        &DiffOptions {
            algorithm: Algorithm::Myers,
            max_work: 10,
            ..opts()
        },
    );
    assert_eq!(d.note, Some(DiffNote::TooExpensive));
    assert_eq!(d.hunks.len(), 1);
    assert_eq!((d.stats.removed, d.stats.added), (200, 200));
    assert_eq!(d.apply(&a).unwrap(), b);
    assert_eq!(diff_text(a.as_str(), b.as_str(), &opts()).note, None);
}

#[test]
fn word_diff_marks_only_what_changed() {
    let (o, n) = word_diff("let x = foo(a, b);", "let x = bar(a, c);").unwrap();
    let cut = |s: &str, spans: &diff::InlineSpans| -> Vec<String> {
        spans
            .0
            .iter()
            .map(|r| s[r.as_range()].to_string())
            .collect()
    };
    assert_eq!(cut("let x = foo(a, b);", &o), ["foo", "b"]);
    assert_eq!(cut("let x = bar(a, c);", &n), ["bar", "c"]);
}

#[test]
fn word_diff_merges_spans_split_by_one_space() {
    let (old, new) = ("aaa bbb ccc ddd eee", "aaa xxx yyy ddd eee");
    let (o, n) = word_diff(old, new).unwrap();
    assert_eq!(o.0.len(), 1);
    assert_eq!(&old[o.0[0].as_range()], "bbb ccc");
    assert_eq!(&new[n.0[0].as_range()], "xxx yyy");
}

#[test]
fn word_diff_skips_pairs_that_share_under_30_percent() {
    assert!(word_diff("completely different", "nothing alike here!").is_none());
    // The spec's own example shares one space of seven bytes, so it is skipped
    // by rule 2 (recorded in tasks/008.DONE.md).
    assert!(word_diff("foo bar", "baz qux").is_none());
}

#[test]
fn fill_inline_pairs_lines_by_position() {
    let a = "keep\nlet x = 1;\nlet y = 2;\nextra\nkeep2\n";
    let b = "keep\nlet x = 9;\nlet y = 8;\nkeep2\n";
    let mut d = diff_text(
        a,
        b,
        &DiffOptions {
            context: 1,
            ..opts()
        },
    );
    assert!(d.hunks[0].lines.iter().all(|l| l.inline.is_none()));
    d.fill_inline(0);
    let lines = &d.hunks[0].lines;
    let with: Vec<_> = lines.iter().filter(|l| l.inline.is_some()).collect();
    assert_eq!(
        with.len(),
        4,
        "two pairs; the unpaired removed line has none"
    );
    let first_removed = lines.iter().find(|l| l.kind == LineKind::Removed).unwrap();
    let span = first_removed.inline.as_ref().unwrap().0[0];
    assert_eq!(&d.line_text(first_removed)[span.as_range()], "1");
    d.fill_inline(0);
    d.fill_inline(99); // out of range: ignored
}

#[test]
fn big_file_with_many_changes_is_fast() {
    // 50k lines with about 6k changed lines (2k each replaced, deleted, inserted);
    // a third of the lines are common ones like braces.
    let mut a = String::new();
    let mut b = String::new();
    for i in 0..50_000u32 {
        let line = if i % 3 == 0 {
            ["}\n", "\n", "    return x;\n", "    }\n"][(i / 3) as usize % 4].to_string()
        } else {
            format!("    let value_{} = compute({}, {});\n", i % 977, i, i % 13)
        };
        a.push_str(&line);
        match i % 25 {
            0 => b.push_str(&format!("    changed_{i}();\n")),
            8 => {}
            16 => {
                b.push_str(&line);
                b.push_str(if i % 2 == 0 {
                    "}\n"
                } else {
                    "    inserted();\n"
                });
            }
            _ => b.push_str(&line),
        }
    }
    let start = std::time::Instant::now();
    let d = diff_text(a.as_str(), b.as_str(), &opts());
    let took = start.elapsed();
    assert_eq!(d.note, None);
    assert!(d.stats.added + d.stats.removed >= 5_000);
    assert_eq!(d.apply(&a).unwrap(), b);
    // Release runs in tens of ms; the bound only catches a complexity blowup.
    let bound = if cfg!(debug_assertions) { 10 } else { 2 };
    assert!(took.as_secs() < bound, "diff took {took:?}");
    eprintln!("50k-line diff: {took:?}");
}
