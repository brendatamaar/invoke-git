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

/// The worktree path of `path`, refused when it could leave the worktree or
/// reach into `.git` (an absolute path, a `..`, a drive or UNC prefix, an
/// empty part).
fn inside(paths: &RepoPaths, path: &RepoPath) -> Result<std::path::PathBuf, String> {
    let bad = path.0.is_empty()
        || path.0.starts_with(['/', '\\'])
        || path.0.contains(':')
        || path.0.split(['/', '\\']).any(|part| {
            part.is_empty() || part == ".." || part == "." || part.eq_ignore_ascii_case(".git")
        });
    if bad {
        return Err(format!("{} is not a path inside the repository", path.0));
    }
    Ok(paths.workdir.join(&path.0))
}

/// The file as it is in the working tree now, with the conflict markers git
/// wrote. `Absent` when there is no file (the side that deleted it won).
/// Text keeps its line endings exactly.
pub fn read_worktree_file(paths: &RepoPaths, path: &RepoPath) -> Result<StageContent, String> {
    let target = inside(paths, path)?;
    match std::fs::read(&target) {
        Ok(bytes) => Ok(classify(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(StageContent::Absent),
        Err(error) => Err(format!("read {}: {error}", path.0)),
    }
}

/// Writes the resolved text of a conflicted file as it is given (the caller
/// has put back the line endings). The file must already exist: this edits a
/// conflicted file, it does not create one.
pub fn write_worktree_file(paths: &RepoPaths, path: &RepoPath, text: &str) -> Result<(), String> {
    let target = inside(paths, path)?;
    if !target.is_file() {
        return Err(format!("{} is not a file in the working tree", path.0));
    }
    std::fs::write(&target, text.as_bytes()).map_err(|error| format!("write {}: {error}", path.0))
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

    fn temp_repo(name: &str) -> RepoPaths {
        let dir = std::env::temp_dir().join(format!("invoke-wt-{name}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        RepoPaths {
            workdir: dir.clone(),
            git_dir: dir.join(".git"),
        }
    }

    #[test]
    fn the_worktree_file_round_trips_byte_for_byte() {
        let paths = temp_repo("rt");
        let path = RepoPath("dir/a.txt".into());
        std::fs::create_dir_all(paths.workdir.join("dir")).unwrap();
        std::fs::write(paths.workdir.join("dir/a.txt"), b"x\r\n<<<<<<< a\r\n").unwrap();
        assert_eq!(
            read_worktree_file(&paths, &path).unwrap(),
            StageContent::Text("x\r\n<<<<<<< a\r\n".into())
        );
        write_worktree_file(&paths, &path, "x\r\nresolved\r\n").unwrap();
        assert_eq!(
            std::fs::read(paths.workdir.join("dir/a.txt")).unwrap(),
            b"x\r\nresolved\r\n"
        );
        assert_eq!(
            read_worktree_file(&paths, &RepoPath("gone.txt".into())).unwrap(),
            StageContent::Absent
        );
        assert!(write_worktree_file(&paths, &RepoPath("gone.txt".into()), "x").is_err());
        std::fs::write(paths.workdir.join("b.bin"), [0u8, 1, 2]).unwrap();
        assert_eq!(
            read_worktree_file(&paths, &RepoPath("b.bin".into())).unwrap(),
            StageContent::Binary { size: 3 }
        );
        let _ = std::fs::remove_dir_all(&paths.workdir);
    }

    #[test]
    fn a_path_that_leaves_the_worktree_is_refused() {
        let paths = temp_repo("esc");
        // A real file next to the repository, which a path with `..` would reach.
        let outside = format!("escape-{}.txt", std::process::id());
        let outside_file = paths.workdir.parent().unwrap().join(&outside);
        std::fs::write(&outside_file, "outside").unwrap();
        let up = format!("../{outside}");
        let up_again = format!("a/../../{outside}");
        for bad in [
            up.as_str(),
            up_again.as_str(),
            "/etc/x",
            "\\x",
            "C:/x",
            ".git/config",
            "dir/.GIT/config",
            "",
            "a//b",
            "./x",
        ] {
            let path = RepoPath(bad.into());
            assert!(read_worktree_file(&paths, &path).is_err(), "{bad}");
            assert!(write_worktree_file(&paths, &path, "x").is_err(), "{bad}");
        }
        assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "outside");
        let _ = std::fs::remove_file(&outside_file);
        let _ = std::fs::remove_dir_all(&paths.workdir);
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
