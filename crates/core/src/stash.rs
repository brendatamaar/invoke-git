//! Stash (backlog task 20): the stash list as read from `refs/stash`'s
//! reflog, and the pure rules for addressing one entry.
//!
//! An entry is addressed by its commit id, never by `stash@{n}` alone: the
//! position moves whenever another stash is saved or dropped, and a request
//! for a stash that moved must be refused (rule 5).

use crate::ids::{Oid, RefName};

/// One entry of the stash list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StashEntry {
    /// The stash commit. This is the identity of the entry.
    pub id: Oid,
    /// The reflog message, for example `On main: wip` or
    /// `WIP on main: 1a2b3c4 subject`.
    pub message: String,
}

/// The ref whose reflog is the stash list.
pub fn stash_ref() -> RefName {
    RefName("refs/stash".into())
}

/// Where `id` sits in `stashes` (newest first, so 0 is `stash@{0}`).
pub fn position(stashes: &[StashEntry], id: &Oid) -> Option<usize> {
    stashes.iter().position(|entry| entry.id == *id)
}

/// What the stash ref points at: the newest entry.
pub fn top(stashes: &[StashEntry]) -> Option<&Oid> {
    stashes.first().map(|entry| &entry.id)
}

/// A stash message as git can store it: one line, no NUL, trimmed. An empty
/// result means "let git write its default message".
pub fn clean_message(message: &str) -> String {
    let first = message.split(['\n', '\r']).next().unwrap_or("");
    first.replace('\0', "").trim().to_owned()
}

/// The label shown for a stash operation.
pub fn label(verb: &str, message: &str) -> String {
    let message = clean_message(message);
    if message.is_empty() {
        verb.to_owned()
    } else {
        format!("{verb} '{message}'")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn cleaned_messages_are_one_clean_line(message in "\\PC*|[a-z \\n\\r\\t\\x00]*") {
            let cleaned = clean_message(&message);
            prop_assert!(!cleaned.contains(['\n', '\r', '\0']));
            prop_assert_eq!(cleaned.trim(), cleaned.as_str());
            prop_assert_eq!(clean_message(&cleaned), cleaned);
        }

        #[test]
        fn position_finds_the_first_entry_with_that_id(n in 0..6usize, probe in 0..8u8) {
            let id = |k: u8| Oid::from_hex(&format!("{k:040x}")).unwrap();
            let stashes: Vec<_> = (0..n as u8)
                .map(|k| StashEntry { id: id(k), message: String::new() })
                .collect();
            let found = position(&stashes, &id(probe));
            prop_assert_eq!(found, (usize::from(probe) < n).then_some(usize::from(probe)));
            prop_assert_eq!(top(&stashes), stashes.first().map(|e| &e.id));
        }
    }
}
