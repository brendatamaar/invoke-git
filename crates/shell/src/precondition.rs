//! What a write expects to find on disk, read fresh just before it runs
//! (rule 5, law 12).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

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

static USER_CONFIG: AtomicBool = AtomicBool::new(false);

/// Makes reads honour the user's and the system's git config, as git does
/// (`core.autocrlf`, `core.excludesFile`, ...). Off by default, so tests are
/// the same on every machine; the app turns it on at start-up. The repo's own
/// config always applies.
pub fn use_user_config(on: bool) {
    USER_CONFIG.store(on, Ordering::Relaxed);
}

pub(crate) fn options() -> gix::open::Options {
    if USER_CONFIG.load(Ordering::Relaxed) {
        gix::open::Options::default()
    } else {
        gix::open::Options::isolated()
    }
}

/// Opens the repo whose own git dir is `paths.git_dir`. See [`use_user_config`]
/// for which config applies.
pub(crate) fn open(paths: &RepoPaths) -> Result<gix::Repository, String> {
    gix::open_opts(&paths.git_dir, options())
        .map_err(|error| format!("open {}: {error}", paths.git_dir.display()))
}

/// Opens the repo through its working tree, which a status read needs.
/// Opening by git dir alone knows no worktree when the git dir is
/// elsewhere (`--separate-git-dir`) or a linked worktree's own.
pub(crate) fn open_with_worktree(paths: &RepoPaths) -> Result<gix::Repository, String> {
    gix::open_opts(&paths.workdir, options())
        .map_err(|error| format!("open {}: {error}", paths.workdir.display()))
}

/// P10: fallback opener when `filter.lfs.*` is configured (with `required=true`)
/// but `git-lfs` is not installed, so read-only status/diff inspection still
/// succeeds instead of failing on the missing external filter process.
pub(crate) fn open_with_worktree_without_lfs_filter(
    paths: &RepoPaths,
) -> Result<gix::Repository, String> {
    let mut repo = open_with_worktree(paths)?;
    {
        let mut cfg = repo.config_snapshot_mut();
        while cfg.remove_section("filter", Some("lfs".into())).is_some() {}
    }
    Ok(repo)
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
    check_values(paths, expect)
}

fn check_values(paths: &RepoPaths, expect: &Expect) -> Result<(), Failure> {
    let refuse = |details| Failure {
        class: ErrorClass::PreconditionChanged,
        details,
    };
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

/// Hold Git's index lock across a guarded write. Git commands inside this
/// guard must use a private index if they need to write it.
pub(crate) struct IndexLock {
    path: Option<PathBuf>,
    file: Option<File>,
}

impl IndexLock {
    pub(crate) fn acquire(paths: &RepoPaths, expect: &Expect) -> Result<Self, Failure> {
        let path = paths.git_dir.join("index.lock");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| Failure {
                class: if error.kind() == io::ErrorKind::AlreadyExists {
                    ErrorClass::PreconditionChanged
                } else {
                    ErrorClass::Environment
                },
                details: format!("{}: {error}", path.display()),
            })?;
        let lock = Self {
            path: Some(path),
            file: Some(file),
        };
        check_values(paths, expect)?;
        Ok(lock)
    }

    pub(crate) fn verify(&self, paths: &RepoPaths, expect: &Expect) -> Result<(), Failure> {
        check_values(paths, expect)
    }

    /// Install an index using Git's lock-and-rename protocol. An empty
    /// snapshot denotes an absent index.
    pub(crate) fn replace(
        mut self,
        git_dir: &Path,
        bytes: &[u8],
        modified: Option<SystemTime>,
    ) -> io::Result<()> {
        let index = git_dir.join("index");
        if bytes.is_empty() {
            return match fs::remove_file(index) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                result => result,
            };
        }
        let mut file = self.file.take().expect("open lock file");
        file.write_all(bytes)?;
        if let Some(modified) = modified {
            file.set_modified(modified)?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(self.path.as_ref().expect("owned lock"), index)?;
        self.path = None;
        Ok(())
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

/// Private scratch space on the same filesystem as a planned rename.
pub(crate) struct ScratchDir(pub PathBuf);

impl ScratchDir {
    pub(crate) fn for_index(paths: &RepoPaths) -> io::Result<Self> {
        Self::for_repo(paths, false)
    }

    pub(crate) fn for_worktree(paths: &RepoPaths) -> io::Result<Self> {
        Self::for_repo(paths, true)
    }

    fn for_repo(paths: &RepoPaths, needs_rename: bool) -> io::Result<Self> {
        let workdir = fs::canonicalize(&paths.workdir)?;
        let git_dir = fs::canonicalize(&paths.git_dir)?;
        let dot_git = paths.workdir.join(".git");
        let ordinary_git_dir = fs::symlink_metadata(&dot_git).is_ok_and(|m| m.is_dir())
            && fs::canonicalize(&dot_git).is_ok_and(|path| path == git_dir);
        if (ordinary_git_dir || !git_dir.starts_with(&workdir))
            && (!needs_rename || same_volume(&git_dir, &workdir)?)
        {
            return Self::create(&paths.git_dir);
        }

        // A separate git dir can be inside the worktree without being
        // protected by Git. Use the system temp directory only if it is
        // outside the worktree and meets the atomic rename requirement.
        let temp = std::env::temp_dir();
        let canonical_temp = fs::canonicalize(&temp)?;
        if !canonical_temp.starts_with(&workdir)
            && (!needs_rename || same_volume(&canonical_temp, &workdir)?)
        {
            return Self::create(&temp);
        }

        Err(io::Error::other(
            "no safe scratch directory outside the worktree on the required volume",
        ))
    }

    pub(crate) fn create(parent: &Path) -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Git children resolve paths from the worktree, including when temp is relative.
        let parent = std::path::absolute(parent)?;
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            // The system temp root can be shared with other users.
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        loop {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".invoke-{}-{id}", std::process::id()));
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(unix)]
fn same_volume(a: &Path, b: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(fs::metadata(a)?.dev() == fs::metadata(b)?.dev())
}

/// Both paths resolve to the same volume mount point. `GetVolumePathNameW`
/// only reads path components, so nothing is opened.
#[cfg(windows)]
fn same_volume(a: &Path, b: &Path) -> io::Result<bool> {
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    let volume = |path: &Path| -> io::Result<Vec<u16>> {
        let wide = verbatim_path(path)?;
        let mut buffer = vec![0u16; wide.len() + 1];
        // SAFETY: `wide` is NUL-terminated and `buffer` holds as many units
        // as the path itself plus one, which bounds the mount point's length.
        let done =
            unsafe { GetVolumePathNameW(wide.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
        if done == 0 {
            return Err(io::Error::last_os_error());
        }
        let end = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        buffer.truncate(end);
        Ok(String::from_utf16_lossy(&buffer)
            .to_lowercase()
            .encode_utf16()
            .collect())
    };
    Ok(volume(a)? == volume(b)?)
}

#[cfg(not(any(unix, windows)))]
fn same_volume(_: &Path, _: &Path) -> io::Result<bool> {
    Ok(false)
}

/// A NUL-terminated Win32 path that also supports paths over MAX_PATH.
#[cfg(windows)]
pub(crate) fn verbatim_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let absolute: Vec<u16> = std::path::absolute(path)?
        .as_os_str()
        .encode_wide()
        .map(|unit| {
            if unit == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                unit
            }
        })
        .collect();
    let units = |text: &str| text.encode_utf16().collect::<Vec<u16>>();
    let mut wide = if absolute.starts_with(&units(r"\\?\")) {
        absolute
    } else if let Some(share) = absolute.strip_prefix(&units(r"\\")[..]) {
        [units(r"\\?\UNC\"), share.to_vec()].concat()
    } else {
        [units(r"\\?\"), absolute].concat()
    };
    wide.push(0);
    Ok(wide)
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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
                // The same value `read_refs` records: a direct ref's own
                // target (for an annotated tag, the tag object, not the
                // commit under it), a symbolic ref's final id.
                let id = match reference.try_id() {
                    Some(id) => id.detach(),
                    None => reference
                        .peel_to_id()
                        .map_err(|error| bug(format!("read {}: {error}", item.name.0)))?
                        .detach(),
                };
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

/// Refuses a stash write when the entry at `stash@{index}` is not the stash
/// with commit id `id` any more (someone saved, popped or dropped one since
/// the core read the list). Read fresh through gix.
pub fn check_stash(paths: &RepoPaths, index: usize, id: &Oid) -> Result<(), Failure> {
    let fail = |class, details: String| Failure { class, details };
    let repo = open(paths).map_err(|error| fail(ErrorClass::Bug, error))?;
    let stashes = crate::read::read_stashes(&repo).map_err(|error| fail(ErrorClass::Bug, error))?;
    match stashes.get(index) {
        Some(entry) if entry.id == *id => Ok(()),
        found => Err(fail(
            ErrorClass::PreconditionChanged,
            format!(
                "stash moved: expected {id} at stash@{{{index}}}, found {}",
                found.map_or("nothing".to_owned(), |entry| entry.id.to_string())
            ),
        )),
    }
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

/// Task 17: deletes `index.lock` in the git dir, the leftover of a git that
/// was killed or crashed. It removes that one file and nothing else, and says
/// whether there was one. The core asks for it only after the user agreed and
/// when none of our own operations is running.
pub fn remove_index_lock(paths: &RepoPaths) -> io::Result<bool> {
    match std::fs::remove_file(paths.git_dir.join("index.lock")) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[test]
    fn only_the_index_lock_is_removed() {
        let dir = std::env::temp_dir().join(format!("invoke-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = RepoPaths {
            workdir: dir.clone(),
            git_dir: dir.clone(),
        };
        assert!(!remove_index_lock(&paths).unwrap());
        for name in ["index.lock", "HEAD.lock", "index"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        assert!(remove_index_lock(&paths).unwrap());
        assert!(!dir.join("index.lock").exists());
        assert!(dir.join("HEAD.lock").exists() && dir.join("index").exists());
        assert!(!remove_index_lock(&paths).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
