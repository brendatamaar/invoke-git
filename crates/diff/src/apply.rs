//! Applying a diff to the old text. This is how law 1 is tested, and a check
//! that a diff still fits the text it will be applied to.

use crate::lines::{full_line, line_count, line_starts};
use crate::model::{LineKind, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyError {
    /// A hunk starts before the end of the previous one, or past the text.
    OutOfOrder { hunk: usize },
    /// A context or removed line is not where the diff says it is.
    Mismatch { hunk: usize, old_no: u32 },
    /// A hunk's line numbers do not match its header.
    BadHeader { hunk: usize },
}

impl TextDiff {
    /// Applies the hunks to `a` and returns the new text.
    ///
    /// Removed lines are checked against `a` byte for byte, line endings
    /// included; added lines come from the new buffer. Fails instead of
    /// guessing when the diff does not fit `a`.
    pub fn apply(&self, a: &str) -> Result<String, ApplyError> {
        let starts = line_starts(a);
        let total = line_count(&starts);
        let mut out = String::with_capacity(self.new.len());
        let mut cursor = 0usize; // next old line to copy, 0-based
        for (h, hunk) in self.hunks.iter().enumerate() {
            let first = if hunk.old_len == 0 {
                hunk.old_start as usize
            } else {
                (hunk.old_start as usize).wrapping_sub(1)
            };
            if first < cursor || first > total {
                return Err(ApplyError::OutOfOrder { hunk: h });
            }
            while cursor < first {
                out.push_str(full_line(a, &starts, cursor));
                cursor += 1;
            }
            let mut consumed = 0u32;
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Added => {
                        out.push_str(self.line_text(line));
                        out.push_str(line.eol.as_str());
                    }
                    LineKind::Context | LineKind::Removed => {
                        let old_no = line.old_no.ok_or(ApplyError::BadHeader { hunk: h })?;
                        if old_no as usize != cursor + 1 || cursor >= total {
                            return Err(ApplyError::Mismatch { hunk: h, old_no });
                        }
                        let actual = full_line(a, &starts, cursor);
                        if line.kind == LineKind::Removed {
                            let expected_ok = actual.len()
                                == line.text.as_range().len() + line.eol.as_str().len()
                                && actual.starts_with(self.line_text(line))
                                && actual.ends_with(line.eol.as_str());
                            if !expected_ok {
                                return Err(ApplyError::Mismatch { hunk: h, old_no });
                            }
                        } else {
                            out.push_str(actual);
                        }
                        cursor += 1;
                        consumed += 1;
                    }
                }
            }
            if consumed != hunk.old_len {
                return Err(ApplyError::BadHeader { hunk: h });
            }
        }
        while cursor < total {
            out.push_str(full_line(a, &starts, cursor));
            cursor += 1;
        }
        Ok(out)
    }
}
