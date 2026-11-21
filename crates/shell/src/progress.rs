//! git's `--progress` output on stderr. git redraws a progress line with
//! `\r` and ends it with `\n`, so both end a line here.

use invoke_core::Progress;

/// Splits a byte stream that arrives in arbitrary chunks into lines.
#[derive(Debug, Default)]
pub struct Lines {
    partial: Vec<u8>,
}

impl Lines {
    /// Feeds one chunk and returns the lines it completed, without their
    /// terminators. Empty lines (`\r\n`, blank lines) are skipped.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &byte in chunk {
            if byte == b'\r' || byte == b'\n' {
                if !self.partial.is_empty() {
                    lines.push(String::from_utf8_lossy(&self.partial).into_owned());
                    self.partial.clear();
                }
            } else {
                self.partial.push(byte);
            }
        }
        lines
    }

    /// The unterminated last line, if any.
    pub fn finish(self) -> Option<String> {
        (!self.partial.is_empty()).then(|| String::from_utf8_lossy(&self.partial).into_owned())
    }
}

/// Parses one progress line:
///
/// - `Receiving objects:  45% (9/20), 1.20 MiB | 2.00 MiB/s` → 9 of 20
/// - `remote: Counting objects: 100% (5/5), done.` → 5 of 5
/// - `Enumerating objects: 5, done.` → 5, total unknown
///
/// Progress labels start with a capital letter, which keeps `fatal:`,
/// `error:`, `warning:` and `hint:` lines out.
pub fn parse(line: &str) -> Option<Progress> {
    let line = line.trim();
    let body = line.strip_prefix("remote: ").unwrap_or(line);
    let (label, rest) = body.split_once(": ")?;
    if !label.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    let rest = rest.trim_start();
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let (number, after) = rest.split_at(digits);
    let number: u64 = number.parse().ok()?;
    let (done, total) = match after.strip_prefix('%') {
        Some(after) => {
            let inner = after.trim_start().strip_prefix('(')?;
            let (fraction, _) = inner.split_once(')')?;
            let (done, total) = fraction.split_once('/')?;
            (done.trim().parse().ok()?, Some(total.trim().parse().ok()?))
        }
        None => (number, None),
    };
    let stage = match line.strip_prefix("remote: ") {
        Some(_) => format!("remote: {label}"),
        None => label.to_owned(),
    };
    Some(Progress { stage, done, total })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn progress(stage: &str, done: u64, total: Option<u64>) -> Option<Progress> {
        Some(Progress {
            stage: stage.into(),
            done,
            total,
        })
    }

    #[test]
    fn real_git_lines_parse() {
        assert_eq!(
            parse("Receiving objects:  45% (9/20), 1.20 MiB | 2.00 MiB/s"),
            progress("Receiving objects", 9, Some(20))
        );
        assert_eq!(
            parse("remote: Counting objects: 100% (5/5), done."),
            progress("remote: Counting objects", 5, Some(5))
        );
        assert_eq!(
            parse("Enumerating objects: 5, done."),
            progress("Enumerating objects", 5, None)
        );
        assert_eq!(
            parse("Resolving deltas: 100% (1234/1234), done."),
            progress("Resolving deltas", 1234, Some(1234))
        );
        assert_eq!(parse("fatal: 128 things went wrong"), None);
        assert_eq!(parse("error: 3 files"), None);
        assert_eq!(parse("To github.com:me/repo.git"), None);
        assert_eq!(parse("Receiving objects:  45%"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn carriage_returns_end_progress_lines() {
        let mut lines = Lines::default();
        assert_eq!(
            lines.push(b"Receiving objects:  1% (1/100)\rReceiv"),
            ["Receiving objects:  1% (1/100)"]
        );
        assert_eq!(
            lines.push(b"ing objects: 100% (100/100), done.\r\n"),
            ["Receiving objects: 100% (100/100), done."]
        );
        assert_eq!(lines.push(b"tail"), Vec::<String>::new());
        assert_eq!(lines.finish().as_deref(), Some("tail"));
    }

    proptest! {
        /// However stderr is chunked, the same lines come out.
        #[test]
        fn chunking_never_changes_the_lines(
            text in "([A-Za-z: %()/0-9]{0,12}[\r\n]{0,2}){0,12}",
            cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..8),
        ) {
            let bytes = text.as_bytes();
            let mut whole = Lines::default();
            let mut expected = whole.push(bytes);
            expected.extend(whole.finish());

            let mut points: Vec<usize> = cuts.iter().map(|cut| cut.index(bytes.len() + 1)).collect();
            points.sort_unstable();
            let mut chunked = Lines::default();
            let mut got = Vec::new();
            let mut start = 0;
            for point in points.into_iter().chain([bytes.len()]) {
                got.extend(chunked.push(&bytes[start..point]));
                start = point;
            }
            got.extend(chunked.finish());
            prop_assert_eq!(got, expected);
        }

        /// The parser is total and reads back what git would print.
        #[test]
        fn formatted_progress_parses_back(
            label in "[A-Z][a-z]{1,8}( [a-z]{1,8})?",
            done in 0..10_000u64,
            extra in 0..10_000u64,
            remote in any::<bool>(),
            percent in any::<bool>(),
        ) {
            let total = done + extra;
            let prefix = if remote { "remote: " } else { "" };
            let line = if percent {
                format!("{prefix}{label}: {:>3}% ({done}/{total}), done.", done * 100 / total.max(1))
            } else {
                format!("{prefix}{label}: {done}, done.")
            };
            let stage = format!("{prefix}{label}");
            let parsed = parse(&line);
            prop_assert_eq!(parsed, progress(&stage, done, percent.then_some(total)));
        }

        #[test]
        fn parse_never_panics(line in "\\PC{0,40}") {
            let _ = parse(&line);
        }
    }
}
