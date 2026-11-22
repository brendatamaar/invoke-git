//! Commit messages (task 14): what the editor may send, what git is given,
//! and what the UI shows about it. Everything here is pure text.
//!
//! **Cleanup.** The core applies git's `whitespace` cleanup itself and the
//! shell asks git for exactly that (`--cleanup=whitespace`), so the stored
//! message is what the user sees and git adds nothing of its own:
//!
//! - line endings become LF (a `\r` never reaches a commit, Windows included);
//! - trailing whitespace goes from every line;
//! - blank lines at the start and the end go, and runs of them become one;
//! - the message ends with exactly one newline.
//!
//! Lines that start with `#` are **kept**. git's default for a message that
//! is not edited (`--file`) keeps them too, and the editor has no template
//! that would need stripping, so `#123 fix` must not lose its subject. Only
//! a template git wrote for us, such as `MERGE_MSG`, is stripped, and only
//! when it becomes the editor's starting text ([`strip_comment_lines`]).
//!
//! **Subject.** The first line. Its length is reported as data
//! ([`SubjectHint`]) and never makes a commit fail.

/// Subject length where a soft warning starts. Counted in `char`s, so a
/// wide character counts as one.
pub const SUBJECT_SOFT_LIMIT: usize = 50;
/// Subject length past which the warning is firm (still not a refusal).
pub const SUBJECT_LIMIT: usize = 72;
/// The most subject characters an undo label shows before it is cut.
pub const LABEL_SUBJECT_MAX: usize = 60;

/// Why text cannot be a commit message. Each is stated to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageProblem {
    /// Nothing was typed.
    Empty,
    /// Only spaces, tabs and line breaks: git would abort with "empty
    /// commit message".
    OnlyWhitespace,
    /// A NUL byte, which a git commit cannot contain.
    ContainsNul,
}

/// A message git can commit: normalized (see the module docs), never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMessage(String);

impl CommitMessage {
    /// The one way in: whatever the editor holds becomes a message, or says
    /// why it cannot.
    pub fn parse(raw: &str) -> Result<Self, MessageProblem> {
        if raw.is_empty() {
            return Err(MessageProblem::Empty);
        }
        if raw.contains('\0') {
            return Err(MessageProblem::ContainsNul);
        }
        let text = normalize(raw);
        if text.is_empty() {
            return Err(MessageProblem::OnlyWhitespace);
        }
        Ok(Self(text))
    }

    /// The text git is given: LF only, one trailing newline.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first line.
    pub fn subject(&self) -> &str {
        split(&self.0).0
    }

    /// Everything after the subject and the blank line below it, if
    /// anything. It ends with a newline.
    pub fn body(&self) -> Option<&str> {
        split(&self.0).1
    }

    /// What an undo point says: `Commit 'Fix login'`. A long subject is cut
    /// on a `char` boundary and ends with an ellipsis.
    pub fn label(&self, verb: &str) -> String {
        let subject = self.subject();
        match subject.char_indices().nth(LABEL_SUBJECT_MAX) {
            Some((cut, _)) => format!("{verb} '{}…'", &subject[..cut]),
            None => format!("{verb} '{subject}'"),
        }
    }
}

/// What git's own `isspace` counts, so the cleanup matches `git stripspace`.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

/// CRLF and a lone CR both become LF.
fn unify_newlines(raw: &str) -> String {
    raw.replace("\r\n", "\n").replace('\r', "\n")
}

/// The cleanup from the module docs. Idempotent, and an empty result means
/// the input held nothing but whitespace.
pub fn normalize(raw: &str) -> String {
    let unified = unify_newlines(raw);
    let mut out = String::with_capacity(unified.len());
    let mut blank_pending = false;
    for line in unified.split('\n') {
        let line = line.trim_end_matches(is_space);
        if line.is_empty() {
            // Blank lines before the first text are dropped, the rest
            // collapse into one that is written only if text follows.
            blank_pending = !out.is_empty();
            continue;
        }
        if blank_pending {
            out.push('\n');
            blank_pending = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Splits a normalized message into its subject and its body. Text that is
/// not normalized is cut the same way, without cleaning it.
pub fn split(text: &str) -> (&str, Option<&str>) {
    let (subject, rest) = match text.split_once('\n') {
        Some((subject, rest)) => (subject, rest),
        None => (text, ""),
    };
    let body = rest.trim_start_matches('\n');
    (subject, (!body.is_empty()).then_some(body))
}

/// How long the subject is, against the two limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectLevel {
    /// Up to [`SUBJECT_SOFT_LIMIT`].
    Fine,
    /// Past [`SUBJECT_SOFT_LIMIT`]: a gentle hint.
    PastSoft,
    /// Past [`SUBJECT_LIMIT`]: a firm warning. The commit is still allowed.
    PastLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubjectHint {
    /// The subject's length in `char`s.
    pub chars: usize,
    pub level: SubjectLevel,
}

/// The hint for what is in the editor now (any draft, not only a valid
/// message), judged on the subject the commit would get.
pub fn subject_hint(draft: &str) -> SubjectHint {
    let text = normalize(draft);
    let chars = split(&text).0.chars().count();
    let level = if chars > SUBJECT_LIMIT {
        SubjectLevel::PastLimit
    } else if chars > SUBJECT_SOFT_LIMIT {
        SubjectLevel::PastSoft
    } else {
        SubjectLevel::Fine
    };
    SubjectHint { chars, level }
}

/// Drops the lines git wrote as instructions (they start with `#`), for a
/// template that becomes the editor's starting text, such as `MERGE_MSG`,
/// which lists the conflicted files that way.
pub fn strip_comment_lines(template: &str) -> String {
    let unified = unify_newlines(template);
    let kept: Vec<&str> = unified
        .split('\n')
        .filter(|line| !line.starts_with('#'))
        .collect();
    normalize(&kept.join("\n"))
}
