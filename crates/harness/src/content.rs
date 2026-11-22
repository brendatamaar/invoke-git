//! File contents the harness writes: edits of what is there, so diffs have
//! several hunks, with LF and CRLF endings, no final newline and unicode.

use proptest::prelude::*;

/// The files the harness plays with. Names cover a folder, a space and
/// unicode (including an emoji).
pub const FILES: [&str; 4] = ["a.txt", "dir/b.txt", "sp ace/ü-日本-😀.txt", "c.txt"];

/// Files that exist, committed, in both repos before the first step.
pub fn initial_files() -> Vec<(&'static str, String)> {
    let a: String = (0..24).map(|i| format!("alpha line {i}\n")).collect();
    let b: String = (0..18).map(|i| format!("bravo line {i}\r\n")).collect();
    let c: String = (0..12).map(|i| format!("ünï {i} 日本語\n")).collect();
    vec![(FILES[0], a), (FILES[1], b), (FILES[2], c)]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    Lf,
    CrLf,
}

impl Eol {
    fn as_str(self) -> &'static str {
        match self {
            Eol::Lf => "\n",
            Eol::CrLf => "\r\n",
        }
    }
}

/// A change to one line.
#[derive(Clone, Debug)]
pub enum LineEdit {
    Insert { at: usize, text: String },
    Replace { at: usize, text: String },
    Delete { at: usize },
}

/// What a write to a file does to it.
#[derive(Clone, Debug)]
pub enum Change {
    /// A new file (or a replaced one) with exactly these lines.
    Whole {
        lines: Vec<String>,
        eol: Eol,
        final_newline: bool,
    },
    /// Edits applied to the current lines; `eol` is used for new lines
    /// when the file has no line ending to copy. `final_newline` flips the
    /// last line's ending on or off.
    Edits {
        edits: Vec<LineEdit>,
        eol: Eol,
        final_newline: Option<bool>,
    },
}

/// Applies `change` to the current content (`None`: the file is absent).
pub fn apply(old: Option<&str>, change: &Change) -> String {
    match change {
        Change::Whole {
            lines,
            eol,
            final_newline,
        } => join(
            lines.iter().map(String::as_str),
            eol.as_str(),
            *final_newline,
        ),
        Change::Edits {
            edits,
            eol,
            final_newline,
        } => {
            let old = old.unwrap_or("");
            let ending = if old.contains("\r\n") {
                "\r\n"
            } else if old.contains('\n') {
                "\n"
            } else {
                eol.as_str()
            };
            let had_final = old.ends_with('\n') || old.is_empty();
            let mut lines: Vec<String> = old
                .split_inclusive('\n')
                .map(|line| line.trim_end_matches(['\n', '\r']).to_owned())
                .collect();
            for edit in edits {
                match edit {
                    LineEdit::Insert { at, text } => {
                        let at = at % (lines.len() + 1);
                        lines.insert(at, text.clone());
                    }
                    LineEdit::Replace { at, text } if !lines.is_empty() => {
                        let at = at % lines.len();
                        lines[at] = text.clone();
                    }
                    LineEdit::Delete { at } if !lines.is_empty() => {
                        let at = at % lines.len();
                        lines.remove(at);
                    }
                    _ => {}
                }
            }
            join(
                lines.iter().map(String::as_str),
                ending,
                final_newline.unwrap_or(had_final),
            )
        }
    }
}

fn join<'a>(lines: impl Iterator<Item = &'a str>, ending: &str, final_newline: bool) -> String {
    let lines: Vec<&str> = lines.collect();
    let mut out = lines.join(ending);
    if final_newline && !lines.is_empty() {
        out.push_str(ending);
    }
    out
}

/// Line texts: mostly short ASCII from a small pool (so edits collide and
/// repeat, which is what stresses a diff), some unicode, some blank.
pub fn line_text() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => "[a-d]{1,3} [0-9]{1,2}",
        2 => Just(String::new()),
        1 => Just("héllo wörld".to_owned()),
        1 => Just("日本語のテキスト".to_owned()),
        1 => Just("emoji 😀 line".to_owned()),
        1 => Just("  indented\ttab ".to_owned()),
    ]
}

pub fn eol() -> impl Strategy<Value = Eol> {
    prop_oneof![3 => Just(Eol::Lf), 1 => Just(Eol::CrLf)]
}

fn line_edit() -> impl Strategy<Value = LineEdit> {
    prop_oneof![
        2 => (0..64usize, line_text()).prop_map(|(at, text)| LineEdit::Insert { at, text }),
        3 => (0..64usize, line_text()).prop_map(|(at, text)| LineEdit::Replace { at, text }),
        2 => (0..64usize).prop_map(|at| LineEdit::Delete { at }),
    ]
}

pub fn change() -> impl Strategy<Value = Change> {
    prop_oneof![
        1 => (
            proptest::collection::vec(line_text(), 0..30),
            eol(),
            any::<bool>(),
        )
            .prop_map(|(lines, eol, final_newline)| Change::Whole {
                lines,
                eol,
                final_newline,
            }),
        5 => (
            proptest::collection::vec(line_edit(), 1..6),
            eol(),
            prop_oneof![3 => Just(None), 1 => any::<bool>().prop_map(Some)],
        )
            .prop_map(|(edits, eol, final_newline)| Change::Edits {
                edits,
                eol,
                final_newline,
            }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_the_files_line_ending() {
        let edited = apply(
            Some("a\r\nb\r\n"),
            &Change::Edits {
                edits: vec![LineEdit::Insert {
                    at: 1,
                    text: "x".into(),
                }],
                eol: Eol::Lf,
                final_newline: None,
            },
        );
        assert_eq!(edited, "a\r\nx\r\nb\r\n");
    }

    #[test]
    fn the_final_newline_can_be_dropped() {
        let edited = apply(
            Some("a\nb\n"),
            &Change::Edits {
                edits: vec![],
                eol: Eol::Lf,
                final_newline: Some(false),
            },
        );
        assert_eq!(edited, "a\nb");
    }
}
