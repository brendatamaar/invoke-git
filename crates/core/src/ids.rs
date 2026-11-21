//! Identifiers and names. Parsing happens here, at the boundary, so the rest
//! of the core only ever holds valid values.

use std::fmt;

/// A git object id as lowercase hex: 40 digits for SHA-1, 64 for SHA-256.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(Box<str>);

/// Why a string is not an object id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OidError {
    /// Not 40 or 64 characters long.
    Length(usize),
    /// Contains a character that is not a hex digit.
    NotHex,
}

impl Oid {
    /// Parses a full hex object id. Uppercase digits are accepted and stored
    /// in lowercase, so two spellings of one id compare equal.
    pub fn from_hex(hex: &str) -> Result<Self, OidError> {
        if hex.len() != 40 && hex.len() != 64 {
            return Err(OidError::Length(hex.len()));
        }
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(OidError::NotHex);
        }
        Ok(Self(hex.to_ascii_lowercase().into_boxed_str()))
    }

    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", &self.0[..12.min(self.0.len())])
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identity of the index file as read from disk. A write that expects one
/// index id is refused by the shell when the file on disk has another.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IndexId {
    /// No index file yet (a fresh repo before the first `git add`).
    Absent,
    /// The checksum git stores in the index file's trailer.
    Checksum(Oid),
}

/// A short branch name such as `main` or `feature/login`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchName(pub String);

/// A full ref name such as `refs/heads/main` or `refs/remotes/origin/main`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RefName(pub String);

impl BranchName {
    pub fn to_ref(&self) -> RefName {
        RefName(format!("refs/heads/{}", self.0))
    }
}

/// A path relative to the worktree root, with `/` separators on every
/// platform (the form git itself uses in the index).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(pub String);

/// Repo identity: root commit id plus normalized remote URL. Computing it is
/// the shell's job; the core only carries it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepoId {
    pub root_commit: Option<Oid>,
    pub remote_url: Option<String>,
}

/// One long operation (a write or network command), numbered by the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(pub u64);

/// One state read, numbered by the core in the order it was requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReadId(pub u64);

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn hex_ids_of_valid_length_parse_and_round_trip(
            hex in "[0-9a-fA-F]{40}|[0-9a-fA-F]{64}"
        ) {
            let oid = Oid::from_hex(&hex).unwrap();
            prop_assert_eq!(oid.as_hex(), hex.to_ascii_lowercase());
            prop_assert_eq!(Oid::from_hex(oid.as_hex()).unwrap(), oid);
        }

        #[test]
        fn anything_else_is_rejected(
            text in prop_oneof!["\\PC{0,70}", "[0-9a-fA-F]{0,70}", "[0-9a-f]{38,41}|[0-9a-f]{63,65}"]
        ) {
            let valid = (text.len() == 40 || text.len() == 64)
                && text.bytes().all(|byte| byte.is_ascii_hexdigit());
            prop_assert_eq!(Oid::from_hex(&text).is_ok(), valid);
        }
    }
}
