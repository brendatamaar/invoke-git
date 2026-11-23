//! Conflict blocks in a file git left half merged (task 23, UI half).
//!
//! Git writes `<<<<<<< current`, the current side, `=======`, the incoming
//! side and `>>>>>>> incoming` into the file; with `merge.conflictStyle =
//! diff3` (or zdiff3) the base follows a `||||||| base` line. This finds
//! those blocks and takes one side of one block, which is what "Accept
//! current / incoming / both" does. Pure text in, text out.
//!
//! Only markers of git's default size (seven characters) are recognised, and
//! only at the start of a line; a block that is not closed is not a block, so
//! a half-typed marker the user is editing is left alone. A line ending is
//! kept exactly as it is, `\r\n` included.

use std::ops::Range;

const SIZE: usize = 7;

/// One side to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Take {
    Current,
    Incoming,
    /// The current side, then the incoming one.
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The bytes of the whole block, from the start of the `<<<<<<<` line to
    /// the end of the `>>>>>>>` line (its line ending included).
    pub range: Range<usize>,
    /// 1-based line numbers of the `<<<<<<<` and `>>>>>>>` lines.
    pub first_line: u32,
    pub last_line: u32,
    /// What follows the markers (`main`, `fix/askpass-pipe`).
    pub current_label: String,
    pub incoming_label: String,
    /// The lines of each side, line endings kept.
    pub current: String,
    pub incoming: String,
    /// The common ancestor's lines, when git wrote them (diff3 style).
    pub base: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    Open,
    Base,
    Middle,
    Close,
}

/// The marker a line is, if any. `line` has no line ending.
fn marker(line: &str) -> Option<(Marker, &str)> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let (kind, ch) = match line.as_bytes().first()? {
        b'<' => (Marker::Open, '<'),
        b'|' => (Marker::Base, '|'),
        b'=' => (Marker::Middle, '='),
        b'>' => (Marker::Close, '>'),
        _ => return None,
    };
    let rest = line.strip_prefix(&ch.to_string().repeat(SIZE))?;
    // After the seven characters comes the end of the line or a space (the
    // label); an eighth marker character is a longer marker, not ours.
    let found = match rest.chars().next() {
        None => Some((kind, "")),
        Some(' ') => Some((kind, rest[1..].trim_end())),
        Some(_) => None,
    };
    // The middle line has no label.
    found.filter(|(kind, label)| *kind != Marker::Middle || label.is_empty())
}

/// Splits `text` into lines, each with the byte range it covers (its line
/// ending included).
fn lines(text: &str) -> impl Iterator<Item = (Range<usize>, &str)> {
    let mut start = 0;
    text.split_inclusive('\n').map(move |full| {
        let range = start..start + full.len();
        start = range.end;
        (range, full.strip_suffix('\n').unwrap_or(full))
    })
}

/// Every closed conflict block of `text`, in order.
pub fn find_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    // The block being read: where it opened and what has been seen.
    struct Open {
        start: usize,
        line: u32,
        label: String,
        current: Range<usize>,
        base: Option<(String, Range<usize>)>,
        incoming: Option<Range<usize>>,
    }
    let mut open: Option<Open> = None;
    for (number, (range, line)) in lines(text).enumerate() {
        let number = number as u32 + 1;
        let found = marker(line);
        match (&mut open, found) {
            (None, Some((Marker::Open, label))) => {
                open = Some(Open {
                    start: range.start,
                    line: number,
                    label: label.to_owned(),
                    current: range.end..range.end,
                    base: None,
                    incoming: None,
                });
            }
            (Some(block), Some((Marker::Base, label)))
                if block.base.is_none() && block.incoming.is_none() =>
            {
                block.current.end = range.start;
                block.base = Some((label.to_owned(), range.end..range.end));
            }
            (Some(block), Some((Marker::Middle, _))) if block.incoming.is_none() => {
                match &mut block.base {
                    Some((_, base)) => base.end = range.start,
                    None => block.current.end = range.start,
                }
                block.incoming = Some(range.end..range.end);
            }
            (Some(block), Some((Marker::Close, label))) if block.incoming.is_some() => {
                let mut incoming = block.incoming.clone().unwrap_or(0..0);
                incoming.end = range.start;
                blocks.push(Block {
                    range: block.start..range.end,
                    first_line: block.line,
                    last_line: number,
                    current_label: block.label.clone(),
                    incoming_label: label.to_owned(),
                    current: text[block.current.clone()].to_owned(),
                    incoming: text[incoming].to_owned(),
                    base: block.base.as_ref().map(|(_, r)| text[r.clone()].to_owned()),
                });
                open = None;
            }
            // A second `<<<<<<<` before the first closed: the first one was
            // not a block; start over from this one.
            (Some(_), Some((Marker::Open, label))) => {
                open = Some(Open {
                    start: range.start,
                    line: number,
                    label: label.to_owned(),
                    current: range.end..range.end,
                    base: None,
                    incoming: None,
                });
            }
            _ => {}
        }
    }
    blocks
}

/// True when `text` still has a conflict block.
pub fn has_blocks(text: &str) -> bool {
    !find_blocks(text).is_empty()
}

/// `text` with `block` replaced by the side(s) asked for. `block` must come
/// from [`find_blocks`] of this same `text`.
pub fn accept(text: &str, block: &Block, take: Take) -> String {
    let kept = match take {
        Take::Current => block.current.clone(),
        Take::Incoming => block.incoming.clone(),
        Take::Both => format!("{}{}", block.current, block.incoming),
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..block.range.start]);
    out.push_str(&kept);
    out.push_str(&text[block.range.end..]);
    out
}

/// `text` with every block resolved the same way.
pub fn accept_all(text: &str, take: Take) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for block in find_blocks(text) {
        out.push_str(&text[at..block.range.start]);
        out.push_str(&match take {
            Take::Current => block.current,
            Take::Incoming => block.incoming,
            Take::Both => format!("{}{}", block.current, block.incoming),
        });
        at = block.range.end;
    }
    out.push_str(&text[at..]);
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const SAMPLE: &str = "use a;\n\n<<<<<<< main\nfn one() {}\n=======\nfn one(x: u8) {}\nfn two() {}\n>>>>>>> fix/two\nend\n";

    #[test]
    fn a_block_is_found_with_its_sides_and_labels() {
        let blocks = find_blocks(SAMPLE);
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!((b.first_line, b.last_line), (3, 8));
        assert_eq!(b.current_label, "main");
        assert_eq!(b.incoming_label, "fix/two");
        assert_eq!(b.current, "fn one() {}\n");
        assert_eq!(b.incoming, "fn one(x: u8) {}\nfn two() {}\n");
        assert_eq!(b.base, None);
        assert_eq!(&SAMPLE[b.range.clone()].lines().count(), &6);
    }

    #[test]
    fn each_choice_replaces_only_the_block() {
        let b = &find_blocks(SAMPLE)[0];
        assert_eq!(
            accept(SAMPLE, b, Take::Current),
            "use a;\n\nfn one() {}\nend\n"
        );
        assert_eq!(
            accept(SAMPLE, b, Take::Incoming),
            "use a;\n\nfn one(x: u8) {}\nfn two() {}\nend\n"
        );
        assert_eq!(
            accept(SAMPLE, b, Take::Both),
            "use a;\n\nfn one() {}\nfn one(x: u8) {}\nfn two() {}\nend\n"
        );
    }

    #[test]
    fn the_diff3_style_keeps_the_base_apart() {
        let text = "<<<<<<< ours\nA\n||||||| base\nO\n=======\nB\n>>>>>>> theirs\n";
        let b = &find_blocks(text)[0];
        assert_eq!((b.current.as_str(), b.incoming.as_str()), ("A\n", "B\n"));
        assert_eq!(b.base.as_deref(), Some("O\n"));
        assert_eq!(accept(text, b, Take::Current), "A\n");
    }

    #[test]
    fn crlf_endings_stay_as_they_are() {
        let text = "x\r\n<<<<<<< a\r\nL\r\n=======\r\nR\r\n>>>>>>> b\r\ny\r\n";
        let b = &find_blocks(text)[0];
        assert_eq!(
            (b.current.as_str(), b.incoming.as_str()),
            ("L\r\n", "R\r\n")
        );
        assert_eq!(accept(text, b, Take::Both), "x\r\nL\r\nR\r\ny\r\n");
    }

    #[test]
    fn a_block_without_a_final_newline_and_an_empty_side_work() {
        let text = "<<<<<<< a\n=======\nR\n>>>>>>> b";
        let b = &find_blocks(text)[0];
        assert_eq!((b.current.as_str(), b.incoming.as_str()), ("", "R\n"));
        assert_eq!(accept(text, b, Take::Incoming), "R\n");
        assert_eq!(accept(text, b, Take::Current), "");
    }

    #[test]
    fn things_that_only_look_like_markers_are_not_blocks() {
        // Unclosed, closed without a middle, a longer marker, an indented one.
        for text in [
            "<<<<<<< a\nx\n=======\ny\n",
            "<<<<<<< a\nx\n>>>>>>> b\n",
            "<<<<<<<< a\nx\n=======\ny\n>>>>>>>> b\n",
            "  <<<<<<< a\nx\n  =======\ny\n  >>>>>>> b\n",
            "=======\n",
            "<<<<<<<a\nx\n=======\ny\n>>>>>>>b\n",
        ] {
            assert!(find_blocks(text).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn an_unclosed_opener_does_not_swallow_the_next_block() {
        let text = "<<<<<<< stray\nkeep\n<<<<<<< a\nL\n=======\nR\n>>>>>>> b\n";
        let blocks = find_blocks(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].first_line, 3);
        assert_eq!(
            accept(text, &blocks[0], Take::Current),
            "<<<<<<< stray\nkeep\nL\n"
        );
    }

    #[test]
    fn several_blocks_are_all_found_in_order() {
        let text =
            "<<<<<<< a\n1\n=======\n2\n>>>>>>> b\nmid\n<<<<<<< a\n3\n=======\n4\n>>>>>>> b\n";
        let blocks = find_blocks(text);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].range.end <= blocks[1].range.start);
        assert_eq!(accept_all(text, Take::Incoming), "2\nmid\n4\n");
        assert!(!has_blocks(&accept_all(text, Take::Both)));
    }

    /// A line that cannot be mistaken for a marker.
    fn plain_line() -> impl Strategy<Value = String> {
        "[a-z ]{0,8}"
    }

    #[derive(Clone, Debug)]
    enum Piece {
        Text(Vec<String>),
        Conflict(Vec<String>, Vec<String>),
    }

    fn pieces() -> impl Strategy<Value = Vec<Piece>> {
        let lines = || proptest::collection::vec(plain_line(), 0..4);
        proptest::collection::vec(
            prop_oneof![
                lines().prop_map(Piece::Text),
                (lines(), lines()).prop_map(|(a, b)| Piece::Conflict(a, b)),
            ],
            0..6,
        )
    }

    fn render(pieces: &[Piece], eol: &str) -> (String, String, String, String) {
        let (mut text, mut current, mut incoming, mut both) =
            (String::new(), String::new(), String::new(), String::new());
        let join =
            |lines: &[String]| -> String { lines.iter().map(|l| format!("{l}{eol}")).collect() };
        for piece in pieces {
            match piece {
                Piece::Text(lines) => {
                    let t = join(lines);
                    for out in [&mut text, &mut current, &mut incoming, &mut both] {
                        out.push_str(&t);
                    }
                }
                Piece::Conflict(a, b) => {
                    let (a, b) = (join(a), join(b));
                    text.push_str(&format!(
                        "<<<<<<< ours{eol}{a}======={eol}{b}>>>>>>> theirs{eol}"
                    ));
                    current.push_str(&a);
                    incoming.push_str(&b);
                    both.push_str(&a);
                    both.push_str(&b);
                }
            }
        }
        (text, current, incoming, both)
    }

    proptest! {
        /// Whatever the file looks like, finding the blocks and taking one
        /// side everywhere gives that side's file, and no marker is left.
        #[test]
        fn taking_a_side_everywhere_gives_that_side(
            pieces in pieces(),
            crlf: bool,
        ) {
            let eol = if crlf { "\r\n" } else { "\n" };
            let (text, current, incoming, both) = render(&pieces, eol);
            let expected = pieces.iter().filter(|p| matches!(p, Piece::Conflict(..))).count();
            prop_assert_eq!(find_blocks(&text).len(), expected);
            prop_assert_eq!(accept_all(&text, Take::Current), current);
            prop_assert_eq!(accept_all(&text, Take::Incoming), incoming);
            prop_assert_eq!(accept_all(&text, Take::Both), both);
        }

        /// Resolving the blocks one at a time, from either end, reaches the
        /// same text as resolving them all at once, and each step removes
        /// exactly one block.
        #[test]
        fn one_block_at_a_time_agrees_with_all_at_once(
            pieces in pieces(),
            from_the_end: bool,
        ) {
            let (text, _, incoming, _) = render(&pieces, "\n");
            let mut now = text;
            loop {
                let blocks = find_blocks(&now);
                let Some(block) = (if from_the_end { blocks.last() } else { blocks.first() }) else {
                    break;
                };
                let next = accept(&now, block, Take::Incoming);
                prop_assert_eq!(find_blocks(&next).len(), blocks.len() - 1);
                now = next;
            }
            prop_assert_eq!(now, incoming);
        }

        /// No input makes it panic or return a range outside the text.
        #[test]
        fn any_text_is_safe(text in "[<=>| a\r\nx]{0,120}") {
            for block in find_blocks(&text) {
                prop_assert!(block.range.end <= text.len());
                prop_assert!(text.is_char_boundary(block.range.start));
                prop_assert!(block.first_line < block.last_line);
            }
            let _ = accept_all(&text, Take::Both);
        }
    }
}
