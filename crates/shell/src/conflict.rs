//! The three versions of a conflicted file (task 23), for the resolver UI:
//! the blobs behind index stages 1 (base), 2 (ours) and 3 (theirs), read
//! through gix. Nothing here writes, and nothing needs the worktree file:
//! that one, with its conflict markers, is the UI's to read and edit.

use gix::object::Kind;
use invoke_core::{ConflictContent, ErrorClass, Failure, RepoPath, StageContent};

use crate::precondition::{self, RepoPaths};

/// How much of a blob is searched for a NUL byte, as git itself does when
/// it decides whether a file is binary.
const BINARY_PROBE: usize = 8000;

/// Reads the base, ours and theirs versions of `path` from the index on
/// disk now. A path with no conflict stages is a `Failure` (the conflict is
/// gone: the caller should read the state again).
pub fn read_conflict_content(
    paths: &RepoPaths,
    path: &RepoPath,
) -> Result<ConflictContent, Failure> {
    let fail = |class, details: String| Failure { class, details };
    let repo = precondition::open(paths).map_err(|details| fail(ErrorClass::Bug, details))?;
    let index = repo
        .open_index()
        .map_err(|error| fail(ErrorClass::Bug, format!("read index entries: {error}")))?;
    let mut ids: [Option<gix::ObjectId>; 3] = [None, None, None];
    for entry in index.entries() {
        let stage = entry.stage_raw();
        if stage != 0
            && entry.path(&index) == path.0.as_bytes()
            && let Some(slot) = ids.get_mut(usize::try_from(stage - 1).unwrap_or(usize::MAX))
        {
            *slot = Some(entry.id);
        }
    }
    if ids.iter().all(Option::is_none) {
        return Err(fail(
            ErrorClass::PreconditionChanged,
            format!("{} has no conflict stages in the index", path.0),
        ));
    }
    let side = |id: &Option<gix::ObjectId>| match id {
        None => Ok(StageContent::Absent),
        Some(id) => stage_content(&repo, *id),
    };
    Ok(ConflictContent {
        path: path.clone(),
        base: side(&ids[0]).map_err(|details| fail(ErrorClass::Bug, details))?,
        ours: side(&ids[1]).map_err(|details| fail(ErrorClass::Bug, details))?,
        theirs: side(&ids[2]).map_err(|details| fail(ErrorClass::Bug, details))?,
    })
}

fn stage_content(repo: &gix::Repository, id: gix::ObjectId) -> Result<StageContent, String> {
    let object = repo
        .find_object(id)
        .map_err(|error| format!("read {id}: {error}"))?;
    // A submodule's stage names a commit, which has no text to show.
    if object.kind != Kind::Blob {
        return Ok(StageContent::Binary { size: 0 });
    }
    Ok(classify(&object.data))
}

/// Text only when it is valid UTF-8 without NUL bytes near the start;
/// anything else is flagged binary. The text is kept byte for byte, so a CRLF
/// file stays CRLF.
pub fn classify(data: &[u8]) -> StageContent {
    let probe = &data[..data.len().min(BINARY_PROBE)];
    if probe.contains(&0) {
        return StageContent::Binary {
            size: data.len() as u64,
        };
    }
    match std::str::from_utf8(data) {
        Ok(text) => StageContent::Text(text.to_owned()),
        Err(_) => StageContent::Binary {
            size: data.len() as u64,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn text_keeps_its_line_endings() {
        assert_eq!(
            classify(b"a\r\nb\r\n"),
            StageContent::Text("a\r\nb\r\n".into())
        );
        assert_eq!(classify(b""), StageContent::Text(String::new()));
    }

    #[test]
    fn nul_bytes_and_bad_utf8_are_binary() {
        assert_eq!(classify(b"ab\0cd"), StageContent::Binary { size: 5 });
        assert_eq!(
            classify(&[0xff, 0xfe, b'a']),
            StageContent::Binary { size: 3 }
        );
    }

    proptest! {
        /// Text round-trips exactly; everything else is flagged with its size.
        #[test]
        fn classification_is_lossless_or_flagged(data in proptest::collection::vec(any::<u8>(), 0..200)) {
            match classify(&data) {
                StageContent::Text(text) => prop_assert_eq!(text.into_bytes(), data),
                StageContent::Binary { size } => prop_assert_eq!(size, data.len() as u64),
                StageContent::Absent => prop_assert!(false, "a blob is never absent"),
            }
        }
    }
}
