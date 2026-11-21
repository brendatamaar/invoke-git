//! Tests for the staging patch builder (backlog task 11, laws 5 to 7 pure
//! side): applying a generated patch to the source text gives exactly the
//! selected change, and header counts always match their bodies.
//!
//! The applier below is written from the unified-diff format alone and
//! shares nothing with the builder: it follows header counts, matches
//! lines byte for byte (line endings included) and checks that the new-side
//! start of every hunk agrees with the lines already produced.

use diff::{
    DiffOptions, Direction, LineKind, Mode, PatchSpec, Pick, TextDiff, build_patch, diff_text,
};
use proptest::prelude::*;

const POOL: [&str; 10] = [
    "", "a", "b", "}", "-x", "--- y", "+z", "é日本", "same", "same",
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
            if i + 1 != self.lines.len() || self.final_newline {
                out.push_str(if *crlf { "\r\n" } else { "\n" });
            }
        }
        out
    }
}

fn text() -> impl Strategy<Value = Text> {
    (
        prop::collection::vec((0..POOL.len(), prop::bool::weighted(0.3)), 0..16),
        prop::bool::weighted(0.6),
    )
        .prop_map(|(lines, final_newline)| Text {
            lines,
            final_newline,
        })
}

/// A text and an edited copy, so hunks are small and numerous.
fn pair() -> impl Strategy<Value = (String, String)> {
    (
        text(),
        prop::collection::vec((any::<prop::sample::Index>(), 0..3u8, 0..POOL.len()), 0..6),
        prop::bool::weighted(0.3),
        prop::bool::weighted(0.1),
    )
        .prop_map(|(a, edits, flip_final, unrelated)| {
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
                    _ if len > 0 => b.lines[at.index(len)] = (pool, false),
                    _ => {}
                }
            }
            if flip_final {
                b.final_newline = !b.final_newline;
            }
            if unrelated {
                b.lines.reverse();
            }
            (a.render(), b.render())
        })
}

/// A selection as per-line booleans, drawn long enough for any diff here.
fn bits() -> impl Strategy<Value = Vec<bool>> {
    prop::collection::vec(prop::bool::weighted(0.5), 64)
}

// ---- the independent patch reader and applier -------------------------

struct PatchHunk {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
    /// (sign, full line including its newline unless a marker followed).
    lines: Vec<(char, String)>,
}

fn parse_range(text: &str) -> (usize, usize) {
    match text.split_once(',') {
        Some((start, len)) => (start.parse().unwrap(), len.parse().unwrap()),
        None => (text.parse().unwrap(), 1),
    }
}

/// Parses every hunk, following the header counts (not the sign of the
/// next line), and fails if counts and bodies disagree.
fn parse(patch: &str) -> Vec<PatchHunk> {
    let lines: Vec<&str> = patch.split('\n').collect();
    assert_eq!(lines.last(), Some(&""), "patch must end with a newline");
    let lines = &lines[..lines.len() - 1];
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(rest) = lines[i].strip_prefix("@@ -") else {
            i += 1;
            continue;
        };
        let (old, rest) = rest.split_once(" +").expect("header");
        let (new, _) = rest.split_once(" @@").expect("header");
        let (old_start, old_len) = parse_range(old);
        let (new_start, new_len) = parse_range(new);
        i += 1;
        let (mut have_old, mut have_new) = (0, 0);
        let mut body: Vec<(char, String)> = Vec::new();
        while have_old < old_len || have_new < new_len {
            let line = lines
                .get(i)
                .unwrap_or_else(|| panic!("hunk body ends early in:\n{patch}"));
            i += 1;
            let sign = line.chars().next().expect("empty body line");
            assert!(
                matches!(sign, ' ' | '-' | '+'),
                "bad body line {line:?} in:\n{patch}"
            );
            if sign != '+' {
                have_old += 1;
            }
            if sign != '-' {
                have_new += 1;
            }
            let mut full = format!("{}\n", &line[1..]);
            if lines.get(i) == Some(&"\\ No newline at end of file") {
                full.pop();
                i += 1;
            }
            body.push((sign, full));
        }
        assert_eq!((have_old, have_new), (old_len, new_len), "counts");
        hunks.push(PatchHunk {
            old_start,
            old_len,
            new_start,
            new_len,
            lines: body,
        });
    }
    hunks
}

fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// Applies the patch strictly, or says why not.
fn apply(patch: &str, source: &str) -> Result<String, String> {
    let file = split_lines(source);
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0;
    for (n, hunk) in parse(patch).iter().enumerate() {
        let first = if hunk.old_len == 0 {
            hunk.old_start
        } else {
            hunk.old_start - 1
        };
        if first < cursor || first > file.len() {
            return Err(format!("hunk {n} out of order"));
        }
        out.extend(file[cursor..first].iter().map(|l| l.to_string()));
        cursor = first;
        let produced = out.len();
        let expect_new = if hunk.new_len == 0 {
            hunk.new_start
        } else {
            hunk.new_start - 1
        };
        if produced != expect_new {
            return Err(format!(
                "hunk {n}: new start says {expect_new} lines before it, {produced} produced"
            ));
        }
        for (sign, line) in &hunk.lines {
            match sign {
                '+' => out.push(line.clone()),
                ' ' | '-' => {
                    if file.get(cursor) != Some(&line.as_str()) {
                        return Err(format!(
                            "hunk {n}: line {} is {:?}, patch expects {line:?}",
                            cursor + 1,
                            file.get(cursor)
                        ));
                    }
                    if *sign == ' ' {
                        out.push(line.clone());
                    }
                    cursor += 1;
                }
                _ => unreachable!(),
            }
        }
    }
    out.extend(file[cursor..].iter().map(|l| l.to_string()));
    let result: String = out.concat();
    // A line without a newline may only end the file.
    if out
        .iter()
        .take(out.len().saturating_sub(1))
        .any(|l| !l.ends_with('\n'))
    {
        return Err(format!("line without newline in the middle: {out:?}"));
    }
    Ok(result)
}

// ---- helpers ----------------------------------------------------------

fn spec(direction: Direction, old: &str, new: &str) -> PatchSpec<'static> {
    PatchSpec {
        path: "f.txt",
        direction,
        old_exists: !old.is_empty(),
        new_exists: !new.is_empty(),
        mode: Mode::Regular,
    }
}

fn make(old: &str, new: &str) -> TextDiff {
    diff_text(old, new, &DiffOptions::default())
}

/// Indexes of changed lines in a hunk, in order.
fn changed(diff: &TextDiff, hunk: usize) -> Vec<usize> {
    diff.hunks[hunk]
        .lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind != LineKind::Context)
        .map(|(i, _)| i)
        .collect()
}

/// The expected result by the table in the spec, walking the whole file.
/// `pick(hunk, line)` says whether a line is selected.
fn oracle(
    diff: &TextDiff,
    direction: Direction,
    pick: &dyn Fn(usize, usize) -> bool,
) -> Vec<String> {
    // Source side = what the patch applies to.
    let source = match direction {
        Direction::Stage => &diff.old,
        Direction::Unstage => &diff.new,
    };
    let file = split_lines(source);
    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0; // source lines consumed
    for (h, hunk) in diff.hunks.iter().enumerate() {
        let first = match direction {
            Direction::Stage => lines_before(hunk.old_start, hunk.old_len),
            Direction::Unstage => lines_before(hunk.new_start, hunk.new_len),
        };
        out.extend(file[cursor..first].iter().map(|l| l.to_string()));
        cursor = first;
        for (i, line) in hunk.lines.iter().enumerate() {
            let full = format!("{}{}", diff.line_text(line), line.eol.as_str());
            let on = pick(h, i);
            match (direction, line.kind) {
                (_, LineKind::Context) => {
                    out.push(full);
                    cursor += 1;
                }
                (Direction::Stage, LineKind::Removed) => {
                    cursor += 1;
                    if !on {
                        out.push(full);
                    }
                }
                (Direction::Stage, LineKind::Added) => {
                    if on {
                        out.push(full);
                    }
                }
                (Direction::Unstage, LineKind::Added) => {
                    cursor += 1;
                    if !on {
                        out.push(full);
                    }
                }
                (Direction::Unstage, LineKind::Removed) => {
                    if on {
                        out.push(full);
                    }
                }
            }
        }
    }
    out.extend(file[cursor..].iter().map(|l| l.to_string()));
    out
}

fn lines_before(start: u32, len: u32) -> usize {
    if len == 0 { start } else { start - 1 }.try_into().unwrap()
}

fn well_formed(lines: &[String]) -> bool {
    lines
        .iter()
        .take(lines.len().saturating_sub(1))
        .all(|l| l.ends_with('\n'))
}

// ---- properties --------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// A line selection applied to the source gives the table's result;
    /// when no text can say that (a newline-less line in the middle) the
    /// patch still applies and leaves a valid file.
    #[test]
    fn line_selection_gives_exactly_the_selected_change(
        (old, new) in pair(),
        on in bits(),
        unstage in any::<bool>(),
    ) {
        let (direction, source_text, diff) = if unstage {
            // HEAD = old, index = new: the diff is old -> new, source is new.
            (Direction::Unstage, new.clone(), make(&old, &new))
        } else {
            (Direction::Stage, old.clone(), make(&old, &new))
        };
        for (h, hunk) in diff.hunks.iter().enumerate() {
            let pick = |hh: usize, i: usize| hh == h && on[i % on.len()];
            let lines: Vec<usize> = (0..hunk.lines.len()).filter(|&i| pick(h, i)).collect();
            let result = build_patch(
                &diff,
                &spec(direction, &old, &new),
                &Pick::Lines { hunk: h, lines: &lines },
            )
            .unwrap();
            let any_changed = lines.iter().any(|&i| hunk.lines[i].kind != LineKind::Context);
            let Some(patch) = result else {
                prop_assert!(!any_changed, "a change was selected but no patch came back");
                continue;
            };
            let expected = oracle(&diff, direction, &pick);
            let applied = apply(&patch, &source_text)
                .unwrap_or_else(|e| panic!("{e}\npatch:\n{patch}\nsource: {source_text:?}"));
            if well_formed(&expected) {
                prop_assert_eq!(applied, expected.concat(), "patch:\n{}", patch);
            }
            // Otherwise the selection was widened (see build_patch docs);
            // `apply` already checked the patch fits and leaves a valid file.
        }
    }

    /// Law 5, pure side: every hunk of a file stage turns the old text into
    /// the new text.
    #[test]
    fn staging_every_hunk_gives_the_new_text((old, new) in pair()) {
        let diff = make(&old, &new);
        let patch = build_patch(&diff, &spec(Direction::Stage, &old, &new), &Pick::File).unwrap();
        match patch {
            None => prop_assert_eq!(&old, &new),
            Some(patch) => {
                let applied = apply(&patch, &old)
                    .unwrap_or_else(|e| panic!("{e}\n{patch}"));
                prop_assert_eq!(applied, new);
            }
        }
    }

    /// Law 6, pure side: unstaging the whole diff gives the old text back.
    #[test]
    fn unstaging_every_hunk_gives_the_old_text((old, new) in pair()) {
        let diff = make(&old, &new);
        let patch = build_patch(&diff, &spec(Direction::Unstage, &old, &new), &Pick::File).unwrap();
        match patch {
            None => prop_assert_eq!(&old, &new),
            Some(patch) => {
                let applied = apply(&patch, &new)
                    .unwrap_or_else(|e| panic!("{e}\n{patch}"));
                prop_assert_eq!(applied, old);
            }
        }
    }

    /// Hunk picks equal picking all of a hunk's changed lines.
    #[test]
    fn a_hunk_pick_equals_all_its_lines((old, new) in pair(), unstage in any::<bool>()) {
        let diff = make(&old, &new);
        let direction = if unstage { Direction::Unstage } else { Direction::Stage };
        for h in 0..diff.hunks.len() {
            let lines = changed(&diff, h);
            let s = spec(direction, &old, &new);
            let a = build_patch(&diff, &s, &Pick::Hunk(h)).unwrap();
            let b = build_patch(&diff, &s, &Pick::Lines { hunk: h, lines: &lines }).unwrap();
            prop_assert_eq!(a, b);
        }
    }

    /// Header counts equal the counted lines for any selection (the parser
    /// follows the counts and asserts they consume the body exactly).
    #[test]
    fn header_counts_match_the_body(
        (old, new) in pair(),
        on in bits(),
        unstage in any::<bool>(),
    ) {
        let diff = make(&old, &new);
        let direction = if unstage { Direction::Unstage } else { Direction::Stage };
        for (h, hunk) in diff.hunks.iter().enumerate() {
            let lines: Vec<usize> = (0..hunk.lines.len()).filter(|&i| on[i % on.len()]).collect();
            if let Some(patch) = build_patch(
                &diff,
                &spec(direction, &old, &new),
                &Pick::Lines { hunk: h, lines: &lines },
            ).unwrap() {
                for hunk in parse(&patch) {
                    let old_n = hunk.lines.iter().filter(|(s, _)| *s != '+').count();
                    let new_n = hunk.lines.iter().filter(|(s, _)| *s != '-').count();
                    prop_assert_eq!((old_n, new_n), (hunk.old_len, hunk.new_len));
                }
            }
        }
    }
}

// ---- examples ----------------------------------------------------------

fn patch_of(old: &str, new: &str, direction: Direction, pick: Pick<'_>) -> Option<String> {
    let diff = make(old, new);
    build_patch(&diff, &spec(direction, old, new), &pick).unwrap()
}

#[test]
fn the_no_newline_marker_follows_the_line_that_lacks_it() {
    let patch = patch_of("a\nb", "a\nc", Direction::Stage, Pick::File).unwrap();
    assert_eq!(
        patch,
        "diff --git a/f.txt b/f.txt\n--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+c\n\\ No newline at end of file\n"
    );
}

#[test]
fn crlf_is_kept_byte_for_byte() {
    let patch = patch_of("a\r\nb\r\n", "a\r\nB\r\n", Direction::Stage, Pick::File).unwrap();
    assert!(patch.contains("\n-b\r\n+B\r\n"), "{patch:?}");
}

#[test]
fn only_context_selected_gives_no_patch() {
    let diff = make("a\nb\nc\n", "a\nB\nc\n");
    let context: Vec<usize> = (0..diff.hunks[0].lines.len())
        .filter(|&i| diff.hunks[0].lines[i].kind == LineKind::Context)
        .collect();
    let patch = build_patch(
        &diff,
        &spec(Direction::Stage, "a\nb\nc\n", "a\nB\nc\n"),
        &Pick::Lines {
            hunk: 0,
            lines: &context,
        },
    )
    .unwrap();
    assert_eq!(patch, None);
}

#[test]
fn out_of_range_picks_are_errors() {
    let diff = make("a\n", "b\n");
    let s = spec(Direction::Stage, "a\n", "b\n");
    assert!(build_patch(&diff, &s, &Pick::Hunk(1)).is_err());
    assert!(
        build_patch(
            &diff,
            &s,
            &Pick::Lines {
                hunk: 0,
                lines: &[99]
            }
        )
        .is_err()
    );
}

/// Staging only `+c` after `b` (no newline) must also stage the newline
/// on `b`; otherwise no text could hold the result.
#[test]
fn staging_a_line_after_an_unterminated_one_adds_its_newline() {
    let old = "a\nb";
    let new = "a\nb\nc\n";
    let diff = make(old, new);
    let hunk = &diff.hunks[0];
    let added_c = hunk
        .lines
        .iter()
        .position(|l| l.kind == LineKind::Added && diff.line_text(l) == "c")
        .unwrap();
    let patch = build_patch(
        &diff,
        &spec(Direction::Stage, old, new),
        &Pick::Lines {
            hunk: 0,
            lines: &[added_c],
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(apply(&patch, old).unwrap(), "a\nb\nc\n");
}

/// Unstaging the removal of an unterminated last line keeps nothing after
/// it: later added lines come out too.
#[test]
fn unstaging_an_unterminated_line_takes_the_lines_after_it() {
    let head = "a\nb";
    let index = "a\nb\nc\n";
    let diff = make(head, index);
    let hunk = &diff.hunks[0];
    let removed_b = hunk
        .lines
        .iter()
        .position(|l| l.kind == LineKind::Removed)
        .unwrap();
    let patch = build_patch(
        &diff,
        &spec(Direction::Unstage, head, index),
        &Pick::Lines {
            hunk: 0,
            lines: &[removed_b],
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(apply(&patch, index).unwrap(), "a\nb");
}

#[test]
fn new_files_and_deleted_files_have_their_headers() {
    let diff = make("", "x\ny\n");
    let s = PatchSpec {
        path: "d/é f.txt",
        direction: Direction::Stage,
        old_exists: false,
        new_exists: true,
        mode: Mode::Executable,
    };
    let patch = build_patch(&diff, &s, &Pick::File).unwrap().unwrap();
    assert_eq!(
        patch,
        "diff --git a/d/é f.txt b/d/é f.txt\nnew file mode 100755\n--- /dev/null\n+++ b/d/é f.txt\t\n@@ -0,0 +1,2 @@\n+x\n+y\n"
    );
    // Unstage of that same added file deletes it from the index.
    let s = PatchSpec {
        direction: Direction::Unstage,
        ..s
    };
    let patch = build_patch(&diff, &s, &Pick::File).unwrap().unwrap();
    assert!(
        patch.contains(
            "deleted file mode 100755\n--- a/d/é f.txt\t\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-x\n-y\n"
        ),
        "{patch}"
    );
}

#[test]
fn an_empty_new_file_is_a_header_only_patch() {
    let diff = make("", "");
    let s = PatchSpec {
        path: "empty",
        direction: Direction::Stage,
        old_exists: false,
        new_exists: true,
        mode: Mode::Regular,
    };
    assert_eq!(
        build_patch(&diff, &s, &Pick::File).unwrap().unwrap(),
        "diff --git a/empty b/empty\nnew file mode 100644\n"
    );
    // Same on both sides: nothing to do.
    let s = PatchSpec {
        old_exists: true,
        ..s
    };
    assert_eq!(build_patch(&diff, &s, &Pick::File).unwrap(), None);
}

#[test]
fn odd_names_are_quoted() {
    let diff = make("a\n", "b\n");
    let s = PatchSpec {
        path: "we\"ird\\n\u{1}.txt",
        direction: Direction::Stage,
        old_exists: true,
        new_exists: true,
        mode: Mode::Regular,
    };
    let patch = build_patch(&diff, &s, &Pick::File).unwrap().unwrap();
    assert!(
        patch.starts_with(
            "diff --git \"a/we\\\"ird\\\\n\\001.txt\" \"b/we\\\"ird\\\\n\\001.txt\"\n"
        ),
        "{patch}"
    );
}
