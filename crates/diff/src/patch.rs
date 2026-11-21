//! Staging patches: a selection of a diff (file, hunk or lines) rewritten
//! into a unified patch for `git apply --cached`, in either direction.
//!
//! The rules are the table in `docs/spec/diff-engine.md` -> "Line staging".
//! Pure text in, text out; the shell applies the result.
//!
//! The patch applies to the *source* side, which is the index in both
//! directions (stage: the diff's old side; unstage: the diff's new side,
//! because the diff is HEAD to index there).
//!
//! The diff must have been made with exact whitespace comparison and
//! without `ignore_blank_lines`: a context line then is the same bytes on
//! both sides, which the rewrite relies on.
//!
//! A line without a final newline is only valid as the last line of a
//! file. Selecting part of a change at the end of such a file can ask for
//! something no text can say (a line without newline followed by another
//! line), so the selection is widened just enough to stay valid; see
//! [`widen`].

use crate::lines::{line_count, line_starts};
use crate::model::{DiffLine, Eol, Hunk, LineKind, TextDiff};

/// Which way the patch moves the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// The diff is index to worktree; selected changes go into the index.
    Stage,
    /// The diff is HEAD to index; selected changes come out of the index.
    Unstage,
}

/// What the user selected. Line numbers are positions in `Hunk::lines`
/// (context lines included; selecting one selects nothing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick<'a> {
    File,
    Hunk(usize),
    Lines { hunk: usize, lines: &'a [usize] },
}

/// The file mode written into the header of a new or deleted file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Regular,
    Executable,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Regular => "100644",
            Mode::Executable => "100755",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PatchSpec<'a> {
    /// Repo-relative path with `/` separators.
    pub path: &'a str,
    pub direction: Direction,
    /// Does the file exist on the diff's old side? (`false` for an added or
    /// untracked file.)
    pub old_exists: bool,
    /// Does the file exist on the diff's new side? (`false` when deleted.)
    pub new_exists: bool,
    /// Used for the header when the patch creates or deletes the file.
    pub mode: Mode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchError {
    NoSuchHunk { hunk: usize },
    NoSuchLine { hunk: usize, line: usize },
}

/// Builds the patch for `pick`, or `None` when the selection changes
/// nothing (only context lines, or a file without a difference).
pub fn build_patch(
    diff: &TextDiff,
    spec: &PatchSpec<'_>,
    pick: &Pick<'_>,
) -> Result<Option<String>, PatchError> {
    let mut selected = mask(diff, pick)?;
    let (source, source_exists, result_exists) = match spec.direction {
        Direction::Stage => (&diff.old, spec.old_exists, spec.new_exists),
        Direction::Unstage => (&diff.new, spec.new_exists, spec.old_exists),
    };

    let mut body = String::new();
    let mut delta: i64 = 0;
    let mut result_lines = line_count(&line_starts(source)) as i64;
    let mut emitted = false;
    for (h, hunk) in diff.hunks.iter().enumerate() {
        if !selected[h].iter().any(|on| *on) {
            continue;
        }
        widen(spec.direction, diff, hunk, &mut selected[h]);
        let lines = rewrite(spec.direction, diff, hunk, &selected[h]);
        let old_len = lines.iter().filter(|l| l.sign != '+').count() as i64;
        let new_len = lines.iter().filter(|l| l.sign != '-').count() as i64;
        let (start, len) = match spec.direction {
            Direction::Stage => (hunk.old_start, hunk.old_len),
            Direction::Unstage => (hunk.new_start, hunk.new_len),
        };
        // Lines before the hunk on the source side (git counts a hunk
        // without source lines from the line before it).
        let before = if len == 0 { start } else { start - 1 } as i64;
        let new_before = before + delta;
        body.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            header_start(before, old_len),
            old_len,
            header_start(new_before, new_len),
            new_len
        ));
        for line in &lines {
            body.push(line.sign);
            body.push_str(line.text);
            match line.eol {
                // CRLF stays CRLF: the carriage return is part of the line.
                Eol::Lf | Eol::CrLf => body.push_str(line.eol.as_str()),
                Eol::None => body.push_str("\n\\ No newline at end of file\n"),
            }
        }
        delta += new_len - old_len;
        result_lines += new_len - old_len;
        emitted = true;
    }

    let creates = !source_exists;
    let deletes = source_exists && !result_exists && result_lines == 0;
    let whole_file_only = !emitted && matches!(pick, Pick::File) && source_exists != result_exists;
    if !emitted && !whole_file_only {
        return Ok(None);
    }
    let path = spec.path;
    let mut patch = format!("diff --git {} {}\n", name("a", path), name("b", path));
    if creates {
        patch.push_str(&format!("new file mode {}\n", spec.mode.as_str()));
        if emitted {
            patch.push_str(&format!("--- /dev/null\n+++ {}\n", header_name("b", path)));
        }
    } else if deletes || whole_file_only {
        patch.push_str(&format!("deleted file mode {}\n", spec.mode.as_str()));
        if emitted {
            patch.push_str(&format!("--- {}\n+++ /dev/null\n", header_name("a", path)));
        }
    } else {
        patch.push_str(&format!(
            "--- {}\n+++ {}\n",
            header_name("a", path),
            header_name("b", path)
        ));
    }
    patch.push_str(&body);
    Ok(Some(patch))
}

/// Selected positions per hunk.
fn mask(diff: &TextDiff, pick: &Pick<'_>) -> Result<Vec<Vec<bool>>, PatchError> {
    let changed = |hunk: &Hunk| {
        hunk.lines
            .iter()
            .map(|line| line.kind != LineKind::Context)
            .collect::<Vec<_>>()
    };
    let mut selected: Vec<Vec<bool>> = diff
        .hunks
        .iter()
        .map(|hunk| vec![false; hunk.lines.len()])
        .collect();
    match pick {
        Pick::File => {
            for (h, hunk) in diff.hunks.iter().enumerate() {
                selected[h] = changed(hunk);
            }
        }
        Pick::Hunk(h) => {
            let hunk = diff
                .hunks
                .get(*h)
                .ok_or(PatchError::NoSuchHunk { hunk: *h })?;
            selected[*h] = changed(hunk);
        }
        Pick::Lines { hunk, lines } => {
            let row = selected
                .get_mut(*hunk)
                .ok_or(PatchError::NoSuchHunk { hunk: *hunk })?;
            let source = &diff.hunks[*hunk];
            for &line in *lines {
                let kind = source.lines.get(line).map(|l| l.kind);
                match kind {
                    None => return Err(PatchError::NoSuchLine { hunk: *hunk, line }),
                    Some(LineKind::Context) => {}
                    Some(_) => row[line] = true,
                }
            }
        }
    }
    Ok(selected)
}

/// Widens a selection that no text could express.
///
/// A line without a final newline can only be the last line of a file.
///
/// * Stage: the old file's last line `b` has no newline and stays in (not
///   selected), yet a selected `+` line follows it. The new line would come
///   after `b`, so `b` must get its newline: select the removal of `b` and
///   the addition of its newline-terminated twin (the first added line with
///   the same text).
/// * Unstage: the removal of the diff's last old line `b` (no newline in
///   HEAD) is selected, so `b` returns without a newline, yet an added line
///   after it stays in the index. Nothing may follow it, so every added line
///   after it is selected too.
fn widen(direction: Direction, diff: &TextDiff, hunk: &Hunk, selected: &mut [bool]) {
    let Some(at) = hunk
        .lines
        .iter()
        .position(|l| l.kind == LineKind::Removed && l.eol == Eol::None)
    else {
        return;
    };
    let after = || (at + 1)..hunk.lines.len();
    let added = |i: usize| hunk.lines[i].kind == LineKind::Added;
    match direction {
        Direction::Stage => {
            if !selected[at] && after().any(|i| added(i) && selected[i]) {
                selected[at] = true;
                let text = diff.line_text(&hunk.lines[at]);
                let twin = after().find(|&i| added(i) && diff.line_text(&hunk.lines[i]) == text);
                if let Some(twin) = twin {
                    selected[twin] = true;
                }
            }
        }
        Direction::Unstage => {
            if selected[at] {
                for i in after().filter(|&i| added(i)) {
                    selected[i] = true;
                }
            }
        }
    }
}

struct Out<'a> {
    sign: char,
    text: &'a str,
    eol: Eol,
}

/// Applies the rewrite table to one hunk, then puts removals before
/// additions inside each run of changes (git's own order).
fn rewrite<'a>(
    direction: Direction,
    diff: &'a TextDiff,
    hunk: &'a Hunk,
    selected: &[bool],
) -> Vec<Out<'a>> {
    let mut out: Vec<Out<'a>> = Vec::with_capacity(hunk.lines.len());
    for (line, on) in hunk.lines.iter().zip(selected) {
        let sign = match (direction, line.kind, *on) {
            (_, LineKind::Context, _) => Some(' '),
            (Direction::Stage, LineKind::Added, true) => Some('+'),
            (Direction::Stage, LineKind::Added, false) => None,
            (Direction::Stage, LineKind::Removed, true) => Some('-'),
            (Direction::Stage, LineKind::Removed, false) => Some(' '),
            (Direction::Unstage, LineKind::Added, true) => Some('-'),
            (Direction::Unstage, LineKind::Added, false) => Some(' '),
            (Direction::Unstage, LineKind::Removed, true) => Some('+'),
            (Direction::Unstage, LineKind::Removed, false) => None,
        };
        if let Some(sign) = sign {
            out.push(piece(diff, line, sign));
        }
    }
    // Removals first inside each run that has no context line in it.
    let mut start = 0;
    while start < out.len() {
        if out[start].sign == ' ' {
            start += 1;
            continue;
        }
        let end = out[start..]
            .iter()
            .position(|o| o.sign == ' ')
            .map_or(out.len(), |n| start + n);
        out[start..end].sort_by_key(|o| o.sign != '-');
        start = end;
    }
    out
}

fn piece<'a>(diff: &'a TextDiff, line: &DiffLine, sign: char) -> Out<'a> {
    Out {
        sign,
        text: diff.line_text(line),
        eol: line.eol,
    }
}

/// Start number for a hunk header from the count of lines before it.
fn header_start(before: i64, len: i64) -> i64 {
    if len == 0 { before } else { before + 1 }
}

/// `a/path` or `b/path`, quoted the way git quotes odd names.
fn name(prefix: &str, path: &str) -> String {
    quote(&format!("{prefix}/{path}"))
}

/// Like [`name`] for `---` and `+++` lines, where a name with a space ends
/// with a tab so git finds where it stops.
fn header_name(prefix: &str, path: &str) -> String {
    let quoted = name(prefix, path);
    if path.contains(' ') && !quoted.starts_with('"') {
        format!("{quoted}\t")
    } else {
        quoted
    }
}

/// C-style quoting for names with quotes, backslashes or control
/// characters. Other UTF-8 stays as it is (`core.quotepath=false` style).
fn quote(name: &str) -> String {
    if !name
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return name.to_owned();
    }
    let mut out = String::from("\"");
    for c in name.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("\\{byte:03o}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
