//! What a write expects to find on disk, read fresh just before it runs
//! (rule 5, law 12).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use invoke_core::{BranchName, ErrorClass, Expect, Failure, Head, IndexId, Oid, RefValue};

/// Where a repo lives. For a linked worktree `git_dir` is the worktree's own
/// git dir, which holds its `HEAD`, `index` and `index.lock`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoPaths {
    pub workdir: PathBuf,
    pub git_dir: PathBuf,
}

/// Reads the current head and index id: the values a write must match.
pub fn current(paths: &RepoPaths) -> Result<Expect, String> {
    let repo = open(paths)?;
    let head = read_head(&repo)?;
    let hash_len = repo.object_hash().len_in_bytes();
    let index = index_id(&paths.git_dir.join("index"), hash_len)
        .map_err(|error| format!("read index: {error}"))?;
    Ok(Expect { head, index })
}

/// Opens the repo whose own git dir is `paths.git_dir`, ignoring the user's
/// and the system's git config.
pub(crate) fn open(paths: &RepoPaths) -> Result<gix::Repository, String> {
    gix::open_opts(&paths.git_dir, gix::open::Options::isolated())
        .map_err(|error| format!("open {}: {error}", paths.git_dir.display()))
}

/// What `HEAD` says, as the core's [`Head`].
pub(crate) fn read_head(repo: &gix::Repository) -> Result<Head, String> {
    let head = repo.head().map_err(|error| format!("read HEAD: {error}"))?;
    Ok(match head.kind {
        gix::head::Kind::Symbolic(reference) => {
            let name = branch_name(reference.name.as_bstr().to_string());
            match reference.target.try_id() {
                Some(id) => Head::Branch {
                    name,
                    oid: oid(id)?,
                },
                None => return Err("HEAD points at a symbolic ref chain".into()),
            }
        }
        gix::head::Kind::Unborn(name) => Head::Unborn(branch_name(name.as_bstr().to_string())),
        gix::head::Kind::Detached { target, .. } => Head::Detached(oid(&target)?),
    })
}

/// The checksum in the index file's trailer: the last `hash_len` bytes.
///
/// With `index.skipHash=true` git writes an all-zero trailer, which never
/// changes. Then the id is built from the file's size and modification time
/// instead (padded with zeros to the hash width), so a changed index still
/// gives a different id. A real checksum starting with 24 zero digits is
/// beyond unlikely, so the two forms cannot be confused.
pub fn index_id(index: &Path, hash_len: usize) -> io::Result<IndexId> {
    let mut file = match File::open(index) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(IndexId::Absent),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    let len = metadata.len();
    if len < hash_len as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "index shorter than its checksum",
        ));
    }
    file.seek(SeekFrom::End(-(hash_len as i64)))?;
    let mut trailer = vec![0; hash_len];
    file.read_exact(&mut trailer)?;
    let hex = if trailer.iter().all(|byte| *byte == 0) {
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let id = format!(
            "{len:016x}{:016x}{:08x}",
            modified.as_secs(),
            modified.subsec_nanos()
        );
        format!("{id:0<width$}", width = hash_len * 2)
    } else {
        trailer.iter().map(|byte| format!("{byte:02x}")).collect()
    };
    Oid::from_hex(&hex)
        .map(IndexId::Checksum)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("{error:?}")))
}

/// Refuses a write when `index.lock` is held or disk moved since the core's
/// snapshot. git takes the lock itself and refuses if someone beats us to
/// it, which [`crate::classify`] reports the same way.
pub fn check(paths: &RepoPaths, expect: &Expect) -> Result<(), Failure> {
    let refuse = |details: String| Failure {
        class: ErrorClass::PreconditionChanged,
        details,
    };
    let lock = paths.git_dir.join("index.lock");
    if lock.exists() {
        return Err(refuse(format!(
            "{} exists: another git process is running",
            lock.display()
        )));
    }
    let found = current(paths).map_err(|error| Failure {
        class: ErrorClass::Bug,
        details: error,
    })?;
    if found.head != expect.head {
        return Err(refuse(format!(
            "HEAD moved: expected {:?}, found {:?}",
            expect.head, found.head
        )));
    }
    if found.index != expect.index {
        return Err(refuse(format!(
            "index changed: expected {:?}, found {:?}",
            expect.index, found.index
        )));
    }
    Ok(())
}

/// Refuses a write when a ref it names is not where the core saw it (`None`:
/// not there at all). Read through gix, fresh.
pub fn check_refs(paths: &RepoPaths, expected: &[RefValue]) -> Result<(), Failure> {
    if expected.is_empty() {
        return Ok(());
    }
    let bug = |details: String| Failure {
        class: ErrorClass::Bug,
        details,
    };
    let repo = open(paths).map_err(bug)?;
    for item in expected {
        let found = match repo.try_find_reference(item.name.0.as_str()) {
            Ok(Some(mut reference)) => {
                let id = reference
                    .peel_to_id()
                    .map_err(|error| bug(format!("read {}: {error}", item.name.0)))?;
                Some(oid(&id).map_err(bug)?)
            }
            Ok(None) => None,
            Err(error) => return Err(bug(format!("read {}: {error}", item.name.0))),
        };
        if found != item.oid {
            return Err(Failure {
                class: ErrorClass::PreconditionChanged,
                details: format!(
                    "{} moved: expected {:?}, found {:?}",
                    item.name.0, item.oid, found
                ),
            });
        }
    }
    Ok(())
}

fn branch_name(full: String) -> BranchName {
    match full.strip_prefix("refs/heads/") {
        Some(short) => BranchName(short.to_owned()),
        None => BranchName(full),
    }
}

pub(crate) fn oid(id: &gix::oid) -> Result<Oid, String> {
    Oid::from_hex(&id.to_hex().to_string()).map_err(|error| format!("{id}: {error:?}"))
}
