use highlight::{Capture, HighlightError, Language, Span, detect, highlight, spans_in};
use proptest::prelude::*;

fn run(lang: Language, text: &str) -> Vec<Span> {
    highlight(lang, text, &|| false).expect("highlight")
}

fn check_invariants(text: &str, spans: &[Span]) {
    let mut prev_end = 0;
    for s in spans {
        assert!(s.start < s.end, "empty span {s:?}");
        assert!(s.end <= text.len(), "span past text {s:?}");
        assert!(
            text.is_char_boundary(s.start),
            "start not on boundary {s:?}"
        );
        assert!(text.is_char_boundary(s.end), "end not on boundary {s:?}");
        assert!(s.start >= prev_end, "unsorted or overlapping at {s:?}");
        prev_end = s.end;
    }
}

/// (language, sample, expected capture, the text it must cover)
const SAMPLES: &[(Language, &str, Capture, &str)] = &[
    (
        Language::Rust,
        "fn main() { let x = 1; }\n",
        Capture::Keyword,
        "fn",
    ),
    (
        Language::Rust,
        "// note\nfn a() {}\n",
        Capture::Comment,
        "// note",
    ),
    (
        Language::TypeScript,
        "const x: number = 1;\n",
        Capture::Keyword,
        "const",
    ),
    (
        Language::TypeScript,
        "let s: string = \"hi\";\n",
        Capture::Type,
        "string",
    ),
    (
        Language::Tsx,
        "const a = <div>{\"hi\"}</div>;\n",
        Capture::String,
        "\"hi\"",
    ),
    (
        Language::JavaScript,
        "function f() { return 1; }\n",
        Capture::Keyword,
        "function",
    ),
    (
        Language::Go,
        "package main\nfunc main() {}\n",
        Capture::Keyword,
        "func",
    ),
    (
        Language::Java,
        "class A { int x = 1; }\n",
        Capture::Keyword,
        "class",
    ),
    (
        Language::Kotlin,
        "fun main() { val x = \"a\" }\n",
        Capture::Keyword,
        "fun",
    ),
    (
        Language::Kotlin,
        "val x = \"a\"\n",
        Capture::String,
        "\"a\"",
    ),
    (
        Language::Python,
        "def f():\n    return 1\n",
        Capture::Keyword,
        "def",
    ),
    (Language::Json, "{\"a\": 1}\n", Capture::Number, "1"),
    (Language::Yaml, "a: \"b\"\n", Capture::String, "\"b\""),
    (Language::Toml, "a = \"b\"\n", Capture::String, "\"b\""),
    (
        Language::Markdown,
        "# Title\n\ntext\n",
        Capture::Keyword,
        "Title",
    ),
    (
        Language::Html,
        "<div class=\"a\"></div>\n",
        Capture::Type,
        "div",
    ),
    (
        Language::Css,
        "a { color: red; } /* c */\n",
        Capture::Comment,
        "/* c */",
    ),
    (
        Language::Sql,
        "SELECT 1 FROM t;\n",
        Capture::Keyword,
        "SELECT",
    ),
    (
        Language::Bash,
        "if true; then echo \"x\"; fi\n",
        Capture::Keyword,
        "if",
    ),
];

#[test]
fn every_language_has_a_sample() {
    for lang in Language::ALL {
        assert!(
            SAMPLES.iter().any(|s| s.0 == lang),
            "no sample for {lang:?}"
        );
    }
}

#[test]
fn each_grammar_highlights_its_sample() {
    for &(lang, text, capture, covered) in SAMPLES {
        let spans = run(lang, text);
        check_invariants(text, &spans);
        let hit = spans
            .iter()
            .any(|s| s.capture == capture && text[s.start..s.end].contains(covered));
        assert!(
            hit,
            "{lang:?}: no {capture:?} span over {covered:?} in {spans:?}"
        );
    }
}

#[test]
fn highlighting_is_deterministic() {
    let text = "fn main() { let s = \"a\"; } // c\n";
    assert_eq!(run(Language::Rust, text), run(Language::Rust, text));
}

#[test]
fn utf8_text_snaps_to_char_boundaries() {
    let text = "let s = \"héllo wörld ✓\"; // ünï\n";
    for lang in Language::ALL {
        check_invariants(text, &run(lang, text));
    }
}

#[test]
fn too_many_bytes_is_refused() {
    let text = " ".repeat(highlight::MAX_BYTES + 1);
    assert_eq!(
        highlight(Language::Rust, &text, &|| false),
        Err(HighlightError::TooLarge)
    );
}

#[test]
fn too_many_lines_is_refused() {
    let text = "\n".repeat(highlight::MAX_LINES);
    assert_eq!(
        highlight(Language::Rust, &text, &|| false),
        Err(HighlightError::TooLarge)
    );
}

#[test]
fn cancel_stops_work() {
    let text = "fn a() {}\n".repeat(20_000);
    assert_eq!(
        highlight(Language::Rust, &text, &|| true),
        Err(HighlightError::Cancelled)
    );
}

#[test]
fn cancel_during_query_stops_work() {
    use std::cell::Cell;
    let text = "fn a() { let x = \"s\"; }\n".repeat(4_000);
    // Count the polls of a full run, then cancel on the very last one. The
    // parse finishes before the query starts, so the last poll is the query's.
    let polls = Cell::new(0usize);
    highlight(Language::Rust, &text, &|| {
        polls.set(polls.get() + 1);
        false
    })
    .expect("full run");
    let total = polls.get();
    assert!(total > 1, "expected several polls, got {total}");
    polls.set(0);
    assert_eq!(
        highlight(Language::Rust, &text, &|| {
            polls.set(polls.get() + 1);
            polls.get() >= total
        }),
        Err(HighlightError::Cancelled)
    );
}

#[test]
fn spans_in_returns_touching_spans() {
    let text = "fn a() {} // c\nfn b() {}\n";
    let spans = run(Language::Rust, text);
    let all = spans_in(&spans, 0..text.len());
    assert_eq!(all, &spans[..]);
    assert!(spans_in(&spans, 3..3).is_empty());
    let first_line = spans_in(&spans, 0..15);
    assert!(first_line.iter().all(|s| s.start < 15));
    assert!(first_line.iter().any(|s| s.capture == Capture::Comment));
    let second = spans_in(&spans, 15..text.len());
    assert!(second.iter().all(|s| s.end > 15));
    assert!(!second.is_empty());
}

// --- detection ---

#[test]
fn detection_examples() {
    assert_eq!(detect("a/b.rs", None, None), Some(Language::Rust));
    assert_eq!(detect("a\\B.RS", None, None), Some(Language::Rust));
    assert_eq!(detect("x.rs", Some("Python"), None), Some(Language::Python));
    assert_eq!(detect("x.rs", Some("Ruby"), None), None);
    assert_eq!(
        detect("run", None, Some("#!/usr/bin/env python3")),
        Some(Language::Python)
    );
    assert_eq!(
        detect("run", None, Some("#!/bin/bash -e")),
        Some(Language::Bash)
    );
    assert_eq!(
        detect("run", None, Some("#!/usr/bin/env -S node --x")),
        Some(Language::JavaScript)
    );
    assert_eq!(detect("run", None, Some("#!/usr/bin/perl")), None);
    assert_eq!(detect("run", None, Some("# hi")), None);
    assert_eq!(detect(".bashrc", None, None), Some(Language::Bash));
    assert_eq!(
        detect("x.rs", None, Some("#!/bin/sh")),
        Some(Language::Rust)
    );
    assert_eq!(detect("Makefile", None, None), None);
    assert_eq!(detect(".rs", None, None), None);
}

const EXTS: &[(&str, Language)] = &[
    ("rs", Language::Rust),
    ("ts", Language::TypeScript),
    ("tsx", Language::Tsx),
    ("js", Language::JavaScript),
    ("go", Language::Go),
    ("java", Language::Java),
    ("kt", Language::Kotlin),
    ("py", Language::Python),
    ("json", Language::Json),
    ("yml", Language::Yaml),
    ("toml", Language::Toml),
    ("md", Language::Markdown),
    ("html", Language::Html),
    ("css", Language::Css),
    ("sql", Language::Sql),
    ("sh", Language::Bash),
];

const LINGUIST: &[(&str, Option<Language>)] = &[
    ("Rust", Some(Language::Rust)),
    ("typescript", Some(Language::TypeScript)),
    ("TSX", Some(Language::Tsx)),
    ("JavaScript", Some(Language::JavaScript)),
    ("Go", Some(Language::Go)),
    ("Java", Some(Language::Java)),
    ("Kotlin", Some(Language::Kotlin)),
    ("Python", Some(Language::Python)),
    ("JSON", Some(Language::Json)),
    ("YAML", Some(Language::Yaml)),
    ("TOML", Some(Language::Toml)),
    ("Markdown", Some(Language::Markdown)),
    ("HTML", Some(Language::Html)),
    ("CSS", Some(Language::Css)),
    ("SQL", Some(Language::Sql)),
    ("Shell", Some(Language::Bash)),
    ("Ruby", None),
    ("", None),
];

const SHEBANGS: &[(&str, Option<Language>)] = &[
    ("#!/bin/sh", Some(Language::Bash)),
    ("#!/usr/bin/env bash", Some(Language::Bash)),
    ("#!/usr/bin/python3.11", Some(Language::Python)),
    ("#!/usr/bin/env node", Some(Language::JavaScript)),
    ("#!/usr/bin/perl", None),
    ("not a shebang", None),
];

proptest! {
    /// Attribute beats extension beats shebang, for every combination.
    #[test]
    fn detection_order(
        dir in "[a-z/]{0,8}",
        stem in "[a-z]{1,6}",
        ext in prop::option::of(0..EXTS.len()),
        junk_ext in prop::option::of("[0-9]{1,3}"),
        attr in prop::option::of(0..LINGUIST.len()),
        shebang in prop::option::of(0..SHEBANGS.len()),
    ) {
        let ext_text = match (ext, &junk_ext) {
            (Some(i), _) => Some(EXTS[i].0.to_string()),
            (None, Some(j)) => Some(j.clone()),
            (None, None) => None,
        };
        let path = match &ext_text {
            Some(e) => format!("{dir}/{stem}.{e}"),
            None => format!("{dir}/{stem}"),
        };
        let first_line = shebang.map(|i| SHEBANGS[i].0);
        let got = detect(&path, attr.map(|i| LINGUIST[i].0), first_line);

        let expected = if let Some(a) = attr {
            LINGUIST[a].1
        } else if let Some(i) = ext {
            Some(EXTS[i].1)
        } else {
            shebang.and_then(|i| SHEBANGS[i].1)
        };
        prop_assert_eq!(got, expected);
    }

    /// Extension case never matters.
    #[test]
    fn detection_ignores_extension_case(i in 0..EXTS.len(), upper in any::<bool>()) {
        let ext = if upper { EXTS[i].0.to_uppercase() } else { EXTS[i].0.to_string() };
        prop_assert_eq!(detect(&format!("f.{ext}"), None, None), Some(EXTS[i].1));
    }

    /// Detection never panics on arbitrary input.
    #[test]
    fn detection_total(path in "\\PC{0,30}", attr in prop::option::of("\\PC{0,10}"), line in prop::option::of("\\PC{0,30}")) {
        let _ = detect(&path, attr.as_deref(), line.as_deref());
    }
}

// --- span invariants ---

const FRAGMENTS: &[&str] = &[
    "fn ",
    "let ",
    "{",
    "}",
    "(",
    ")",
    "\"",
    "'",
    "/*",
    "*/",
    "//",
    "<",
    ">",
    "</",
    "#",
    "\n",
    " ",
    "1",
    "0x",
    "é",
    "✓",
    "日本",
    "\u{1F600}",
    "def ",
    ":",
    "=",
    "[",
    "]",
    "```",
    "---",
    "SELECT ",
    "if ",
    "fi",
    "$",
    "${",
    "`",
    "\\",
    "\r\n",
    "\t",
];

fn text_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        "\\PC{0,200}",
        prop::collection::vec(prop::sample::select(FRAGMENTS), 0..60).prop_map(|v| v.concat()),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// For every grammar and any text (valid or not), spans are non-empty,
    /// sorted, non-overlapping, in range and on char boundaries.
    #[test]
    fn spans_are_well_formed(lang in 0..Language::ALL.len(), text in text_strategy()) {
        let spans = highlight(Language::ALL[lang], &text, &|| false).expect("highlight");
        check_invariants(&text, &spans);
    }

    /// `spans_in` returns exactly the spans that touch the range.
    #[test]
    fn spans_in_matches_filter(text in text_strategy(), a in 0usize..300, b in 0usize..300) {
        let spans = highlight(Language::Rust, &text, &|| false).expect("highlight");
        let range = a.min(b)..a.max(b);
        let expected: Vec<Span> = spans
            .iter()
            .copied()
            .filter(|s| s.start < range.end && s.end > range.start)
            .collect();
        prop_assert_eq!(spans_in(&spans, range), &expected[..]);
    }
}
