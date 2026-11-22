//! Task 14: the pure commit-message logic. A message reaches git cleaned,
//! with LF only, and its subject, hint and label are right for any text.

use invoke_core::message::{
    CommitMessage, LABEL_SUBJECT_MAX, MessageProblem, SUBJECT_LIMIT, SUBJECT_SOFT_LIMIT,
    SubjectLevel, normalize, split, strip_comment_lines, subject_hint,
};
use proptest::prelude::*;

/// What git's `isspace` counts.
const SPACE: [char; 6] = [' ', '\t', '\n', '\x0b', '\x0c', '\r'];

/// Text full of the awkward parts: every kind of line break, trailing
/// spaces, blank runs, `#`, accents, CJK and an emoji.
fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => "[a-z# \\t\\r\\n\\x0b\\x0c日é😀]{0,60}",
        2 => any::<String>(),
    ]
    // NUL is its own refusal, covered below.
    .prop_map(|text| text.replace('\0', ""))
}

fn is_space(c: char) -> bool {
    SPACE.contains(&c)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Cleaning twice changes nothing more.
    #[test]
    fn normalize_is_idempotent(raw in text()) {
        let once = normalize(&raw);
        prop_assert_eq!(normalize(&once), once);
    }

    /// The shape of a cleaned message: LF only, no trailing space on a line,
    /// no blank line at the start, at the end or twice in a row, and one
    /// newline at the end.
    #[test]
    fn normalized_text_has_the_cleaned_shape(raw in text()) {
        let out = normalize(&raw);
        prop_assert!(!out.contains('\r'));
        if out.is_empty() {
            return Ok(());
        }
        prop_assert!(out.ends_with('\n'));
        prop_assert!(!out.ends_with("\n\n"));
        prop_assert!(!out.starts_with('\n'));
        prop_assert!(!out.contains("\n\n\n"));
        for line in out.split_terminator('\n') {
            prop_assert!(!line.ends_with(is_space), "{line:?}");
        }
    }

    /// Cleaning removes whitespace and nothing else: every other character
    /// survives, in order (so `#` lines and unicode are kept).
    #[test]
    fn normalize_keeps_every_non_space_character(raw in text()) {
        let kept = |text: &str| text.chars().filter(|c| !is_space(*c)).collect::<String>();
        prop_assert_eq!(kept(&normalize(&raw)), kept(&raw));
    }

    /// Windows line endings give the same message as Unix ones, and a lone
    /// CR (old Mac) is a line break too.
    #[test]
    fn crlf_and_cr_equal_lf(raw in "[a-z \\n]{0,40}") {
        let lf = normalize(&raw);
        prop_assert_eq!(normalize(&raw.replace('\n', "\r\n")), lf.clone());
        prop_assert_eq!(normalize(&raw.replace('\n', "\r")), lf);
    }

    /// Parsing succeeds exactly when something is left after cleaning, and
    /// what it holds is the cleaned text, never a `\r`.
    #[test]
    fn parse_accepts_exactly_what_has_content(raw in text()) {
        let cleaned = normalize(&raw);
        match CommitMessage::parse(&raw) {
            Ok(message) => {
                prop_assert!(!cleaned.is_empty());
                prop_assert_eq!(message.as_str(), cleaned.as_str());
                prop_assert!(!message.as_str().contains('\r'));
            }
            Err(problem) => {
                prop_assert!(cleaned.is_empty());
                let expected = if raw.is_empty() {
                    MessageProblem::Empty
                } else {
                    MessageProblem::OnlyWhitespace
                };
                prop_assert_eq!(problem, expected);
            }
        }
    }

    /// Whitespace-only text is refused with its own reason.
    #[test]
    fn whitespace_only_is_refused(raw in "[ \\t\\r\\n\\x0b\\x0c]{1,20}") {
        prop_assert_eq!(CommitMessage::parse(&raw), Err(MessageProblem::OnlyWhitespace));
    }

    /// A NUL anywhere is refused, whatever else the text holds.
    #[test]
    fn nul_is_refused(before in "[a-z ]{0,5}", after in "[a-z ]{0,5}") {
        let raw = format!("{before}\0{after}");
        prop_assert_eq!(CommitMessage::parse(&raw), Err(MessageProblem::ContainsNul));
    }

    /// Subject and body together are the message, and the subject is one
    /// non-empty line.
    #[test]
    fn subject_and_body_split_the_message(raw in text()) {
        let Ok(message) = CommitMessage::parse(&raw) else {
            return Ok(());
        };
        let (subject, body) = (message.subject(), message.body());
        prop_assert!(!subject.is_empty());
        prop_assert!(!subject.contains('\n'));
        prop_assert!(message.as_str().starts_with(subject));
        if let Some(body) = body {
            prop_assert!(!body.is_empty());
            prop_assert!(!body.starts_with('\n'));
            prop_assert!(body.ends_with('\n'));
            prop_assert!(message.as_str().ends_with(body));
        }
        let rejoined = format!("{subject}{}", body.unwrap_or(""));
        let kept = |text: &str| text.chars().filter(|c| !is_space(*c)).collect::<String>();
        prop_assert_eq!(kept(&rejoined), kept(message.as_str()));
        prop_assert_eq!(split(message.as_str()), (subject, body));
    }

    /// The hint counts `char`s of the subject the commit would get, and the
    /// levels switch exactly past 50 and past 72.
    #[test]
    fn subject_hint_levels_follow_the_limits(n in 1usize..200, wide in any::<bool>()) {
        let draft = format!("{}\n\nbody", if wide { "日" } else { "x" }.repeat(n));
        let hint = subject_hint(&draft);
        prop_assert_eq!(hint.chars, n);
        let expected = if n > SUBJECT_LIMIT {
            SubjectLevel::PastLimit
        } else if n > SUBJECT_SOFT_LIMIT {
            SubjectLevel::PastSoft
        } else {
            SubjectLevel::Fine
        };
        prop_assert_eq!(hint.level, expected);
    }

    /// The hint is about the subject, not the whole draft: a long body, or
    /// blank lines above the subject, change nothing.
    #[test]
    fn subject_hint_ignores_body_and_leading_blanks(
        subject in "[a-z]{1,80}",
        body in "[a-z ]{0,200}",
        blanks in "([ ]{0,3}\\n){0,3}",
    ) {
        let plain = subject_hint(&subject);
        let draft = format!("{blanks}{subject}\n\n{body}");
        prop_assert_eq!(subject_hint(&draft), plain);
    }

    /// The undo label is one line, quotes the first line of the message,
    /// and cuts a long subject on a `char` boundary.
    #[test]
    fn labels_quote_the_subject_and_never_split_a_char(raw in text()) {
        let Ok(message) = CommitMessage::parse(&raw) else {
            return Ok(());
        };
        let label = message.label("Commit");
        prop_assert!(!label.contains('\n') && !label.contains('\r'));
        prop_assert!(label.starts_with("Commit '") && label.ends_with('\''));
        let quoted = &label["Commit '".len()..label.len() - 1];
        let subject = message.subject();
        if subject.chars().count() <= LABEL_SUBJECT_MAX {
            prop_assert_eq!(quoted, subject);
        } else {
            let cut: String = subject.chars().take(LABEL_SUBJECT_MAX).collect();
            prop_assert_eq!(quoted, format!("{cut}…"));
        }
    }

    /// A long subject of multi-byte characters is cut on a `char` boundary
    /// (a byte index would land inside one and panic).
    #[test]
    fn long_multibyte_labels_are_cut_on_a_char_boundary(
        subject in "[a-zé日😀]{55,120}",
        verb in prop_oneof![Just("Commit"), Just("Amend"), Just("Conclude merge")],
    ) {
        let message = CommitMessage::parse(&subject).unwrap();
        let label = message.label(verb);
        let cut: String = subject.chars().take(LABEL_SUBJECT_MAX).collect();
        let expected = if subject.chars().count() > LABEL_SUBJECT_MAX {
            format!("{verb} '{cut}…'")
        } else {
            format!("{verb} '{subject}'")
        };
        prop_assert_eq!(label, expected);
    }

    /// Lines git wrote for us (`#...`) are dropped from a template, and the
    /// rest is cleaned; lines that merely contain `#` stay.
    #[test]
    fn template_comments_are_stripped(raw in text()) {
        let stripped = strip_comment_lines(&raw);
        for line in stripped.lines() {
            prop_assert!(!line.starts_with('#'), "{line:?}");
        }
        prop_assert_eq!(normalize(&stripped), stripped);
    }
}

#[test]
fn examples() {
    let parse = |raw: &str| CommitMessage::parse(raw).unwrap();
    assert_eq!(
        parse("  Fix login  \r\n\r\n\r\n\r\nBody  \r\n\r\n").as_str(),
        "  Fix login\n\nBody\n"
    );
    assert_eq!(parse("#123 fix").as_str(), "#123 fix\n");
    assert_eq!(parse("a\rb").as_str(), "a\nb\n");
    assert_eq!(CommitMessage::parse(""), Err(MessageProblem::Empty));
    assert_eq!(
        CommitMessage::parse(" \r\n\t"),
        Err(MessageProblem::OnlyWhitespace)
    );
    assert_eq!(
        CommitMessage::parse("a\0b"),
        Err(MessageProblem::ContainsNul)
    );

    // Subject and body.
    let message = parse("Fix login\n\nWhy.\n\nMore.");
    assert_eq!(message.subject(), "Fix login");
    assert_eq!(message.body(), Some("Why.\n\nMore.\n"));
    assert_eq!(parse("Only a subject").body(), None);
    // No blank line under the subject: the first line is still the subject.
    assert_eq!(parse("Subject\nsecond").subject(), "Subject");
    assert_eq!(parse("Subject\nsecond").body(), Some("second\n"));

    // Labels.
    assert_eq!(
        parse("Fix login\n\nBody").label("Commit"),
        "Commit 'Fix login'"
    );
    assert_eq!(parse("\n\n  \nAmend me").label("Amend"), "Amend 'Amend me'");
    assert_eq!(parse("ünï 日本 😀").label("Commit"), "Commit 'ünï 日本 😀'");
    let long = "😀".repeat(100);
    assert_eq!(
        parse(&long).label("Commit"),
        format!("Commit '{}…'", "😀".repeat(LABEL_SUBJECT_MAX))
    );
    let exact = "é".repeat(LABEL_SUBJECT_MAX);
    assert_eq!(parse(&exact).label("Commit"), format!("Commit '{exact}'"));

    // Hints at the edges.
    let hint = |n: usize| subject_hint(&"x".repeat(n));
    assert_eq!(hint(50).level, SubjectLevel::Fine);
    assert_eq!(hint(51).level, SubjectLevel::PastSoft);
    assert_eq!(hint(72).level, SubjectLevel::PastSoft);
    assert_eq!(hint(73).level, SubjectLevel::PastLimit);
    assert_eq!(subject_hint("").chars, 0);
    assert_eq!(subject_hint("").level, SubjectLevel::Fine);

    // A merge template: the instructions go, the message stays.
    assert_eq!(
        strip_comment_lines("Merge branch 'x'\n\n# Conflicts:\n#\ta.txt\n"),
        "Merge branch 'x'\n"
    );
}
