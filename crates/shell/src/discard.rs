//! Discard (backlog task 13), the system-git half: `ApplyPatchToWorktree`,
//! and the saving and restoring of a discarded file for undo.
//!
//! The patch is built in the pure `diff` crate (`Direction::Discard`); the
//! core decides when it may run. This module only checks what the request
//! expects to find (rule 5), applies the patch to the worktree and keeps the
//! file's content so the discard can be undone.
//!
//! What is kept, under `refs/invoke-git/undo/` so `git gc` leaves it alone:
//!
//! * `<op>-discard`: a tree holding the file as it was before the discard,
//!   as a blob plus its mode (`git hash-object -w --no-filters`, a private
//!   index file and `git write-tree`). An empty tree means the file did not
//!   exist. Taken by `UndoRunner::snapshot`, before the patch runs.
//! * `<op>-discard-after`: the same for the file as the discard left it,
//!   taken once the patch has applied. An undo writes the saved content back
//!   only while the file still holds exactly this (never over someone
//!   else's change).

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use invoke_core::{
    DiscardRestore, ErrorClass, Event, Expect, Failure, FileVersion, GitArgs, IndexId, Oid, OpId,
    OpOutcome, Patch, RepoPath, WorktreeMode,
};

use crate::git::{GLOBAL_ARGS, Git, invocation};
use crate::precondition::{self, RepoPaths};
use crate::runner::{Control, run_child};
use crate::staging::blob_id;
use crate::undo::{UndoRunner, discard_after_ref, discard_ref};

pub struct DiscardRunner {
    git: Git,
    undo: UndoRunner,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
}

impl DiscardRunner {
    pub fn new(git: Git, paths: RepoPaths) -> Self {
        Self {
            undo: UndoRunner::new(git.clone(), paths.clone()),
            git,
            paths,
            env: Vec::new(),
        }
    }

    /// Extra environment for every git run (tests use it to isolate config).
    pub fn with_env(mut self, env: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        let env: Vec<_> = env.into_iter().collect();
        self.undo = self.undo.with_env(env.clone());
        self.env.extend(env);
        self
    }

    /// Runs `Command::ApplyPatchToWorktree`: the `OpFinished` for `op`.
    ///
    /// Refuses unless the index is `expect_index` and the file holds the
    /// bytes `expect_file` names, and unless the content saved by the
    /// snapshot is those same bytes. Then the patch is checked and applied
    /// with `git apply` (no `--index`: the index is never touched), and the
    /// result is recorded for undo.
    pub fn apply(
        &self,
        op: OpId,
        patch: &Patch,
        path: &RepoPath,
        expect_index: &IndexId,
        expect_file: Option<&Oid>,
    ) -> Event {
        Event::OpFinished(
            op,
            self.apply_outcome(op, patch, path, expect_index, expect_file),
        )
    }

    pub(crate) fn discard_path(
        &self,
        op: OpId,
        args: &GitArgs,
        path: &RepoPath,
        expect: &Expect,
        file: Option<&FileVersion>,
    ) -> OpOutcome {
        match self.write_path(op, args, path, expect, file) {
            Ok(()) => OpOutcome::Succeeded,
            Err(failure) => OpOutcome::Failed(failure),
        }
    }

    fn write_path(
        &self,
        op: OpId,
        args: &GitArgs,
        path: &RepoPath,
        expect: &Expect,
        file: Option<&FileVersion>,
    ) -> Result<(), Failure> {
        let stale = |details| Failure {
            class: ErrorClass::PreconditionChanged,
            details,
        };
        let bug = |details| Failure {
            class: ErrorClass::Bug,
            details,
        };
        let environment = |error: io::Error| Failure {
            class: ErrorClass::Environment,
            details: format!("discard {}: {error}", path.0),
        };
        let lock = precondition::IndexLock::acquire(&self.paths, expect)?;
        let full = worktree_path(&self.paths.workdir, path).map_err(stale)?;
        let repo = precondition::open_with_worktree(&self.paths).map_err(bug)?;
        let hash = repo.object_hash();
        let now = || -> Result<Option<FileVersion>, Failure> {
            worktree_path(&self.paths.workdir, path).map_err(stale)?;
            file_version(&full, path, hash).map_err(stale)
        };
        let changed = || stale(format!("{} changed before discard", path.0));
        if now()?.as_ref() != file
            || self
                .undo
                .read_saved(&discard_ref(op), path)
                .map_err(bug)?
                .as_ref()
                != file
        {
            return Err(changed());
        }
        let tracked = checkout_entry(&repo, path).map_err(stale)?;
        // Prepare checkout using Git's smudge / EOL conversion. Record the
        // resulting bytes and mode for undo before replacing the real file.
        let scratch = tracked
            .is_some()
            .then(|| precondition::ScratchDir::for_worktree(&self.paths))
            .transpose()
            .map_err(environment)?;
        let staged = if let Some(scratch) = &scratch {
            let mut prefix = OsString::from("--prefix=");
            prefix.push(&scratch.0);
            prefix.push("/");
            let mut run = invocation(args);
            run.args.insert(GLOBAL_ARGS.len() + 1, prefix);
            // The scratch copy sits a few short folders deeper than the file
            // itself (`.git/.invoke-*/`); Git for Windows must not refuse it
            // at `MAX_PATH` when the real path fits. Only this private
            // checkout uses it.
            run.args
                .splice(0..0, ["-c", "core.longpaths=true"].map(OsString::from));
            #[cfg(windows)]
            let indexed_link = tracked
                .as_ref()
                .is_some_and(|entry| entry.mode == gix::index::entry::Mode::SYMLINK);
            #[cfg(windows)]
            if indexed_link {
                // Git for Windows opens the link target to infer its type.
                // Check out the raw target text and create the link ourselves.
                run.args
                    .splice(0..0, ["-c", "core.symlinks=false"].map(OsString::from));
            }
            let mut command = self.git.command(&self.paths.workdir);
            command
                .args(run.args)
                .envs(self.env.iter().cloned())
                .env("GIT_INDEX_FILE", self.paths.git_dir.join("index"));
            let done = run_child(command, None, None, &Control::default()).map_err(environment)?;
            if !done.success {
                return Err(Failure {
                    class: ErrorClass::Environment,
                    details: done.output,
                });
            }
            let staged = scratch.0.join(&path.0);
            #[cfg(windows)]
            if indexed_link {
                // Git for Windows defaults to emulation except when MSYS
                // requests native symlinks. Explicit Git config still wins.
                let msys = self
                    .env
                    .iter()
                    .rev()
                    .find(|(key, _)| key.eq_ignore_ascii_case("MSYS"))
                    .map(|(_, value)| value.clone())
                    .or_else(|| std::env::var_os("MSYS"));
                let default = if msys.is_some_and(|value| {
                    value.to_string_lossy().contains("winsymlinks:nativestrict")
                }) {
                    "--default=true"
                } else {
                    "--default=false"
                };
                let symlinks = self
                    .undo
                    .run(
                        &["config", "--type=bool", default, "--get", "core.symlinks"],
                        &[],
                        None,
                    )
                    .map_err(|details| Failure {
                        class: ErrorClass::Environment,
                        details,
                    })?;
                if symlinks.trim() == "true" {
                    materialize_link(&staged, &self.paths.workdir, path).map_err(environment)?;
                }
            }
            Some(staged)
        } else {
            None
        };
        let after = match &staged {
            Some(staged) => Some(
                read_file(staged, path)
                    .map_err(bug)?
                    .ok_or_else(|| bug(format!("Git did not check out {}", path.0)))?,
            ),
            None => None,
        };
        self.undo
            .save_entry(&discard_after_ref(op), path, after.as_ref())
            .map_err(bug)?;
        let scratch = match scratch {
            Some(scratch) => scratch,
            None => precondition::ScratchDir::for_worktree(&self.paths).map_err(environment)?,
        };
        lock.verify(&self.paths, expect)?;
        locate(&self.paths.workdir, path)?;
        install_file(staged.as_deref(), &full, scratch, path, file, hash)
    }

    fn apply_outcome(
        &self,
        op: OpId,
        patch: &Patch,
        path: &RepoPath,
        expect_index: &IndexId,
        expect_file: Option<&Oid>,
    ) -> OpOutcome {
        let stale = |details: String| failed(ErrorClass::PreconditionChanged, details);
        let full = match locate(&self.paths.workdir, path) {
            Ok(full) => full,
            Err(failure) => return OpOutcome::Failed(failure),
        };
        if self.paths.git_dir.join("index.lock").exists() {
            return stale("index.lock exists: another git process is running".into());
        }
        match precondition::current(&self.paths) {
            Ok(found) if found.index == *expect_index => {}
            Ok(found) => {
                return stale(format!(
                    "index changed: expected {expect_index:?}, found {:?}",
                    found.index
                ));
            }
            Err(details) => return failed(ErrorClass::Bug, details),
        }
        let hash = match precondition::open(&self.paths) {
            Ok(repo) => repo.object_hash(),
            Err(details) => return failed(ErrorClass::Bug, details),
        };
        let before = match version_on_disk(&full, path, hash) {
            Ok(Some(file)) if file.mode == WorktreeMode::Symlink => {
                return stale(format!("{} is now a symlink", path.0));
            }
            Ok(before) => before,
            Err(failure) => return OpOutcome::Failed(failure),
        };
        let found = before.as_ref().map(|file| &file.blob);
        if found != expect_file {
            return stale(format!(
                "{} changed since the diff was made: expected {expect_file:?}, found {found:?}",
                path.0
            ));
        }
        // What the snapshot saved must be what is about to be discarded, or
        // an undo would bring back something else.
        match self.undo.read_saved(&discard_ref(op), path) {
            Ok(saved) if saved == before => {}
            Ok(saved) => {
                return stale(format!(
                    "{} changed while it was saved for undo: expected {expect_file:?}, saved {:?}",
                    path.0,
                    saved.map(|file| file.blob)
                ));
            }
            Err(details) => return failed(ErrorClass::Bug, details),
        }
        // Same flags both times, as `git add -p` does. `nowarn` keeps CRLF
        // lines from producing whitespace warnings (or errors, when the
        // user's config says `apply.whitespace=error`).
        for check in [true, false] {
            let mut command = self.git.command(&self.paths.workdir);
            command
                .args(GLOBAL_ARGS)
                .args(["apply", "--whitespace=nowarn"])
                .envs(self.env.iter().map(|(key, value)| (key, value)));
            if check {
                command.arg("--check");
            }
            command.arg("-");
            let done = match run_child(
                command,
                Some(patch.0.as_bytes().to_vec()),
                None,
                &Control::default(),
            ) {
                Ok(done) => done,
                Err(error) => {
                    return failed(ErrorClass::Environment, format!("start git: {error}"));
                }
            };
            if !done.success {
                // The file's bytes matched, so a patch that does not apply
                // is one we built wrong (or that git refuses, such as a
                // binary file); nothing was changed.
                return failed(ErrorClass::Bug, done.output);
            }
        }
        // The file is changed now. Record what it holds, so an undo can tell
        // later whether it is still the discard's result.
        if let Err(Failure { details, .. }) = self.undo.save_file(&discard_after_ref(op), path) {
            return failed(
                ErrorClass::Bug,
                format!(
                    "{} was discarded, but its result could not be recorded, so this \
                     discard cannot be undone: {details}",
                    path.0
                ),
            );
        }
        OpOutcome::Succeeded
    }
}

/// The file as it is on disk, or `None` when it does not exist.
pub(crate) struct OnDisk {
    pub bytes: Vec<u8>,
    pub mode: WorktreeMode,
}

/// The versions a whole-file discard confirmation is built against.
pub struct FileInputs {
    pub based_on: IndexId,
    pub file: Option<FileVersion>,
    pub tracked: bool,
}

pub fn file_inputs(paths: &RepoPaths, path: &RepoPath) -> Result<FileInputs, String> {
    let full = worktree_path(&paths.workdir, path)?;
    let repo = precondition::open_with_worktree(paths)?;
    let hash = repo.object_hash();
    let id = || {
        precondition::index_id(&paths.git_dir.join("index"), hash.len_in_bytes())
            .map_err(|error| format!("read index: {error}"))
    };
    let based_on = id()?;
    let tracked = checkout_entry(&repo, path)?.is_some();
    let file = file_version(&full, path, hash)?;
    if id()? != based_on {
        return Err("The index changed while reading the file. Try again.".into());
    }
    Ok(FileInputs {
        based_on,
        file,
        tracked,
    })
}

pub(crate) fn index_entry(
    repo: &gix::Repository,
    path: &RepoPath,
) -> Result<Option<gix::index::Entry>, String> {
    use gix::index::entry::{Mode, Stage};
    let index = repo
        .index_or_empty()
        .map_err(|error| format!("read index: {error}"))?;
    if index
        .prefixed_entries_range(format!("{}/", path.0).as_str().into())
        .is_some()
    {
        return Err(format!("{} is a directory in the index", path.0));
    }
    for (end, _) in path.0.match_indices('/') {
        if index.entry_by_path((&path.0[..end]).into()).is_some() {
            return Err(format!("{} replaces another indexed path", path.0));
        }
    }
    let entry = index.entry_by_path_and_stage(path.0.as_str().into(), Stage::Unconflicted);
    if entry.is_some_and(|entry| {
        !matches!(
            entry.mode,
            Mode::FILE | Mode::FILE_EXECUTABLE | Mode::SYMLINK
        )
    }) {
        return Err(format!("{} is not a file or symlink in the index", path.0));
    }
    Ok(entry.cloned())
}

fn checkout_entry(
    repo: &gix::Repository,
    path: &RepoPath,
) -> Result<Option<gix::index::Entry>, String> {
    Ok(index_entry(repo, path)?.filter(|entry| {
        !entry
            .flags
            .contains(gix::index::entry::Flags::INTENT_TO_ADD)
    }))
}

/// `path` under `workdir`, refusing anything that could leave the repo or
/// reach `.git`: a discard (and its undo) writes and deletes files.
pub(crate) fn worktree_path(workdir: &Path, path: &RepoPath) -> Result<PathBuf, String> {
    locate(workdir, path).map_err(|failure| failure.details)
}

/// [`worktree_path`], with what kind of failure it is: a path the core
/// should never have built is a bug; a folder on the way that is now a
/// link, a file, or named in another case on disk is a changed
/// precondition; a lookup the system refuses is the environment.
fn locate(workdir: &Path, path: &RepoPath) -> Result<PathBuf, Failure> {
    check_repo_path(path).map_err(|details| Failure {
        class: ErrorClass::Bug,
        details,
    })?;
    let mut full = workdir.to_path_buf();
    let mut parts = path.0.split('/').peekable();
    while let Some(part) = parts.next() {
        full.push(part);
        if parts.peek().is_some() {
            // `is_dir` is false for symlinks and junctions (name-surrogate
            // reparse points); a OneDrive placeholder folder is a folder.
            match fs::symlink_metadata(&full) {
                Ok(metadata) if metadata.is_dir() => same_name_on_disk(&full, path)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(Failure {
                        class: ErrorClass::PreconditionChanged,
                        details: outside(path),
                    });
                }
                Err(error) => {
                    return Err(Failure {
                        class: ErrorClass::Environment,
                        details: format!("{}: {error}", path.0),
                    });
                }
            }
        }
    }
    Ok(full)
}

/// `path` as text: normalized, relative, and not naming `.git` (or what
/// Windows would read as it). Nothing on disk is looked at, so it is all an
/// index-only write (unstage) needs.
pub(crate) fn check_repo_path(path: &RepoPath) -> Result<(), String> {
    let bad = |part: &str| {
        part.is_empty()
            || part == "."
            || part == ".."
            || part.eq_ignore_ascii_case(".git")
            || part.contains('\0')
            || cfg!(windows)
                && (part.contains(['\\', ':'])
                    || part.ends_with(['.', ' '])
                    || part.eq_ignore_ascii_case("git~1"))
    };
    if path.0.split('/').any(bad) {
        return Err(outside(path));
    }
    Ok(())
}

fn outside(path: &RepoPath) -> String {
    format!("{:?} is not a path inside the working tree", path.0)
}

/// A leaf may be a file, a symlink or absent. Directory pathspecs would
/// affect multiple paths and cannot use a single-file undo.
pub(crate) fn check_leaf(full: &Path, path: &RepoPath) -> Result<Option<fs::Metadata>, String> {
    leaf(full, path).map_err(|failure| failure.details)
}

/// [`check_leaf`], with what kind of failure it is: something on disk that
/// is not what it was (`PreconditionChanged`) or a read that failed
/// (`Environment`).
fn leaf(full: &Path, path: &RepoPath) -> Result<Option<fs::Metadata>, Failure> {
    let fail = |class, details| Failure { class, details };
    // A cloud placeholder or dedup stub is the file it stands for.
    match fs::symlink_metadata(full) {
        Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_file() => {
            same_name_on_disk(full, path)?;
            Ok(Some(metadata))
        }
        Ok(_) => Err(fail(
            ErrorClass::PreconditionChanged,
            format!("{} is not a file or symlink", path.0),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(fail(
            ErrorClass::Environment,
            format!("{}: {error}", path.0),
        )),
    }
}

/// Pins each existing component while Git reads a Windows worktree path.
#[cfg(windows)]
pub(crate) fn stage_path_guard(
    workdir: &Path,
    path: &RepoPath,
) -> Result<(Vec<fs::File>, Option<PathBuf>), String> {
    let mut full = workdir.to_path_buf();
    let mut guards = Vec::new();
    let mut parts = path.0.split('/').peekable();
    while let Some(part) = parts.next() {
        full.push(part);
        let guard = match open_guarded(&full) {
            Ok(guard) => guard,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok((guards, Some(full)));
            }
            Err(error) => return Err(format!("{}: {error}", path.0)),
        };
        if parts.peek().is_some() {
            let metadata = guard
                .metadata()
                .map_err(|error| format!("{}: {error}", path.0))?;
            if !metadata.is_dir() {
                return Err(outside(path));
            }
            same_name_on_disk(&full, path).map_err(|failure| failure.details)?;
        } else {
            check_leaf(&full, path)?;
        }
        guards.push(guard);
    }
    Ok((guards, None))
}

#[cfg(windows)]
fn open_guarded(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    };

    // Inspect links themselves and prevent rename or reparse changes while
    // the handle is held. Access to read only attributes would not enforce
    // the sharing restrictions on Windows.
    let mut wide = precondition::verbatim_path(path)?;
    wide.pop();
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(OsString::from_wide(&wide))
}

/// On a case-insensitive filesystem `foo.txt` also finds `Foo.txt`. Only
/// the entry Git names is hashed or replaced: a name that differs on disk
/// is refused.
#[cfg(windows)]
fn same_name_on_disk(full: &Path, path: &RepoPath) -> Result<(), Failure> {
    let (found, reparse_tag) = disk_entry(full).map_err(|error| Failure {
        class: ErrorClass::Environment,
        details: format!("{}: {error}", path.0),
    })?;
    let expected = full.file_name().unwrap_or_default();
    if found.as_os_str() == expected {
        // Rust reports junctions as directory symlinks. Git can traverse
        // them during add, and the undo format cannot restore their type.
        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
        if reparse_tag == IO_REPARSE_TAG_MOUNT_POINT {
            return Err(Failure {
                class: ErrorClass::PreconditionChanged,
                details: format!("{} is a Windows junction", path.0),
            });
        }
        return Ok(());
    }
    let what = if expected == std::ffi::OsStr::new(&path.0) {
        path.0.clone()
    } else {
        format!("{} (in {})", expected.to_string_lossy(), path.0)
    };
    Err(Failure {
        class: ErrorClass::PreconditionChanged,
        details: format!(
            "{what} is named {} on disk. Rename it to match Git first.",
            found.to_string_lossy()
        ),
    })
}

#[cfg(not(windows))]
fn same_name_on_disk(_: &Path, _: &RepoPath) -> Result<(), Failure> {
    Ok(())
}

/// The on-disk name and reparse tag of the directory entry.
/// `FindFirstFileW` reads the parent's entry and does not open the file, so
/// a cloud placeholder is not downloaded.
#[cfg(windows)]
fn disk_entry(full: &Path) -> io::Result<(OsString, u32)> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FindClose, FindFirstFileW, WIN32_FIND_DATAW,
    };

    let wide = precondition::verbatim_path(full)?;
    // SAFETY: `wide` is NUL-terminated, `data` is a plain struct written by
    // the call, and the search handle is closed right away.
    let data = unsafe {
        let mut data: WIN32_FIND_DATAW = std::mem::zeroed();
        let handle = FindFirstFileW(wide.as_ptr(), &mut data);
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        FindClose(handle);
        data
    };
    let name = &data.cFileName;
    let end = name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(name.len());
    let tag = if data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        data.dwReserved0
    } else {
        0
    };
    Ok((OsString::from_wide(&name[..end]), tag))
}

pub(crate) fn read_file(full: &Path, path: &RepoPath) -> Result<Option<OnDisk>, String> {
    read_disk(full, path).map_err(|failure| failure.details)
}

fn read_disk(full: &Path, path: &RepoPath) -> Result<Option<OnDisk>, Failure> {
    let Some(metadata) = leaf(full, path)? else {
        return Ok(None);
    };
    let error = |error: io::Error| Failure {
        class: if error.kind() == io::ErrorKind::NotFound {
            ErrorClass::PreconditionChanged
        } else {
            ErrorClass::Environment
        },
        details: format!("{}: {error}", path.0),
    };
    if metadata.file_type().is_symlink() {
        return Ok(Some(OnDisk {
            bytes: read_link(full, &metadata).map_err(error)?,
            mode: WorktreeMode::Symlink,
        }));
    }
    let bytes = fs::read(full).map_err(error)?;
    Ok(Some(OnDisk {
        bytes,
        mode: if is_executable(&metadata) {
            WorktreeMode::Executable
        } else {
            WorktreeMode::Regular
        },
    }))
}

fn file_version(
    full: &Path,
    path: &RepoPath,
    hash: gix::hash::Kind,
) -> Result<Option<FileVersion>, String> {
    version_on_disk(full, path, hash).map_err(|failure| failure.details)
}

/// [`file_version`], keeping the kind of failure from [`read_disk`].
fn version_on_disk(
    full: &Path,
    path: &RepoPath,
    hash: gix::hash::Kind,
) -> Result<Option<FileVersion>, Failure> {
    read_disk(full, path)?
        .map(|file| {
            Ok(FileVersion {
                blob: blob_id(hash, &file.bytes).map_err(|details| Failure {
                    class: ErrorClass::Bug,
                    details,
                })?,
                mode: file.mode,
            })
        })
        .transpose()
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o100 != 0
}

/// Windows has no executable bit; git records files as `100644` there.
#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn read_link(full: &Path, _: &fs::Metadata) -> io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(fs::read_link(full)?.as_os_str().as_bytes().to_vec())
}

#[cfg(windows)]
fn read_link(full: &Path, metadata: &fs::Metadata) -> io::Result<Vec<u8>> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::FileTypeExt;
    let target = fs::read_link(full)?;
    // Windows distinguishes file and directory symlinks even when dangling.
    let mut bytes = vec![u8::from(metadata.file_type().is_symlink_dir())];
    bytes.extend(target.as_os_str().encode_wide().flat_map(u16::to_le_bytes));
    Ok(bytes)
}

#[cfg(unix)]
fn write_link(bytes: &[u8], link: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(bytes), link)
}

#[cfg(windows)]
fn write_link(bytes: &[u8], link: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::fs::{symlink_dir, symlink_file};
    let (kind, bytes) = bytes.split_first().ok_or(io::ErrorKind::InvalidData)?;
    let (units, remainder) = bytes.as_chunks::<2>();
    if !remainder.is_empty() || *kind > 1 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let target: Vec<u16> = units.iter().copied().map(u16::from_le_bytes).collect();
    let target = OsString::from_wide(&target);
    if *kind == 1 {
        symlink_dir(target, link)
    } else {
        symlink_file(target, link)
    }
}

impl UndoRunner {
    /// Saves the worktree file `path` as a tree under `ref_name` (the file
    /// as a blob with its mode; an empty tree when the file does not
    /// exist). The bytes are stored as they are, without any filter.
    ///
    /// A path that no longer leads to a file (a folder in its place, a
    /// parent that became a file or link) is a changed precondition; a read
    /// the system refuses is the environment; only failing to record what
    /// was read is a bug.
    pub(crate) fn save_file(&self, ref_name: &str, path: &RepoPath) -> Result<(), Failure> {
        let full = worktree_path(&self.paths.workdir, path).map_err(|details| Failure {
            class: ErrorClass::PreconditionChanged,
            details,
        })?;
        let file = read_disk(&full, path)?;
        self.save_entry(ref_name, path, file.as_ref())
            .map_err(|details| Failure {
                class: ErrorClass::Bug,
                details,
            })
    }

    fn save_entry(
        &self,
        ref_name: &str,
        path: &RepoPath,
        file: Option<&OnDisk>,
    ) -> Result<(), String> {
        let scratch = precondition::ScratchDir::for_index(&self.paths)
            .map_err(|error| format!("create snapshot directory: {error}"))?;
        let private = scratch.0.join("index");
        let env = [("GIT_INDEX_FILE", private.as_os_str())];
        if let Some(file) = file {
            let blob = self.save_blob(&file.bytes)?;
            let mode = match file.mode {
                WorktreeMode::Regular => "100644",
                WorktreeMode::Executable => "100755",
                WorktreeMode::Symlink => "120000",
            };
            self.run(
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    mode,
                    blob.trim(),
                    &path.0,
                ],
                &env,
                None,
            )?;
        }
        let tree = self.run(&["write-tree"], &env, None)?;
        self.run(&["update-ref", ref_name, tree.trim()], &[], None)?;
        Ok(())
    }

    /// The file saved under `ref_name`: `None` for an empty tree (the file
    /// did not exist).
    fn read_saved(&self, ref_name: &str, path: &RepoPath) -> Result<Option<FileVersion>, String> {
        use gix::object::tree::EntryKind;
        let repo = precondition::open(&self.paths)?;
        let mut reference = repo.find_reference(ref_name).map_err(|e| e.to_string())?;
        let tree = reference.peel_to_tree().map_err(|e| e.to_string())?;
        if tree.data.is_empty() {
            return Ok(None);
        }
        let entry = tree
            .lookup_entry_by_path(path.0.as_str())
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{ref_name} does not contain {}", path.0))?;
        let mode = match entry.mode().kind() {
            EntryKind::Blob => WorktreeMode::Regular,
            EntryKind::BlobExecutable => WorktreeMode::Executable,
            EntryKind::Link => WorktreeMode::Symlink,
            _ => return Err(format!("{ref_name} does not contain a file snapshot")),
        };
        Ok(Some(FileVersion {
            blob: precondition::oid(&entry.object_id())?,
            mode,
        }))
    }

    /// Undo of a discard: writes the saved content back, if the file still
    /// holds what the discard left. Verify the moved entry before replacing it.
    pub(crate) fn restore_discard(&self, restore: &DiscardRestore) -> OpOutcome {
        let path = &restore.path;
        let full = match worktree_path(&self.paths.workdir, path) {
            Ok(full) => full,
            Err(details) => return failed(ErrorClass::Bug, details),
        };
        let repo = match precondition::open(&self.paths) {
            Ok(repo) => repo,
            Err(details) => return failed(ErrorClass::Bug, details),
        };
        let (saved, after) = match (
            self.read_saved(&discard_ref(restore.discarded), path),
            self.read_saved(&discard_after_ref(restore.discarded), path),
        ) {
            (Ok(saved), Ok(after)) => (saved, after),
            (Err(details), _) | (_, Err(details)) => {
                return failed(
                    ErrorClass::Bug,
                    format!("the saved copy of the discarded file is gone: {details}"),
                );
            }
        };
        let hash = repo.object_hash();
        match version_on_disk(&full, path, hash) {
            Ok(found) if found == after => {}
            Ok(_) => {
                return failed(
                    ErrorClass::PreconditionChanged,
                    format!(
                        "{} changed after the discard. Undo requires the discarded version.",
                        path.0
                    ),
                );
            }
            Err(failure) => return OpOutcome::Failed(failure),
        }
        let environment =
            |error: io::Error| failed(ErrorClass::Environment, format!("{}: {error}", path.0));
        let scratch = match precondition::ScratchDir::for_worktree(&self.paths) {
            Ok(scratch) => scratch,
            Err(error) => return environment(error),
        };
        let temporary = match saved {
            Some(file) => {
                let object = match gix::ObjectId::from_hex(file.blob.as_hex().as_bytes())
                    .map_err(|error| error.to_string())
                    .and_then(|id| repo.find_object(id).map_err(|error| error.to_string()))
                {
                    Ok(object) => object,
                    Err(details) => return failed(ErrorClass::Bug, details),
                };
                let temporary = scratch.0.join("file");
                if let Err(error) = write_file(&temporary, &object.data, file.mode) {
                    return environment(error);
                }
                Some(temporary)
            }
            None => None,
        };
        if let Err(failure) = locate(&self.paths.workdir, path) {
            return OpOutcome::Failed(failure);
        }
        if let Err(failure) = install_file(
            temporary.as_deref(),
            &full,
            scratch,
            path,
            after.as_ref(),
            hash,
        ) {
            return OpOutcome::Failed(failure);
        }
        if temporary.is_none() {
            remove_empty_parents(&full, &self.paths.workdir);
        }
        OpOutcome::Succeeded
    }
}

/// Materialize Git's emulated link in private scratch. Only the bounded
/// worktree lookup below can influence the kind of native link created.
#[cfg(windows)]
fn materialize_link(staged: &Path, workdir: &Path, path: &RepoPath) -> io::Result<()> {
    use std::os::windows::fs::{symlink_dir, symlink_file};

    let bytes = fs::read(staged)?;
    let text = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
    let target = text.replace('/', "\\");
    let target = Path::new(&target);
    fs::remove_file(staged)?;
    if wants_dir_link(workdir, path, target) {
        symlink_dir(target, staged)
    } else {
        symlink_file(target, staged)
    }
}

/// Whether a link at `link` to `target` is a directory link, decided only
/// inside the worktree: the target must be relative, stay inside the repo
/// when its `..` are applied as text, and lead to a real folder through real
/// folders (no link is followed). An absolute, drive-relative or UNC target
/// is never looked at (no network access), and gets Git's default, a file
/// link.
#[cfg(windows)]
fn wants_dir_link(workdir: &Path, link: &RepoPath, target: &Path) -> bool {
    use std::path::Component;
    let mut parts: Vec<&std::ffi::OsStr> = link.0.split('/').map(std::ffi::OsStr::new).collect();
    parts.pop();
    for component in target.components() {
        match component {
            Component::Normal(part) => {
                if part
                    .to_str()
                    .is_none_or(|part| part.contains([':', '\0']) || part.ends_with(['.', ' ']))
                {
                    return false;
                }
                parts.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return false;
                }
            }
            Component::Prefix(_) | Component::RootDir => return false,
        }
    }
    let mut full = workdir.to_path_buf();
    let mut guards = Vec::new();
    for part in parts {
        full.push(part);
        let Ok(guard) = open_guarded(&full) else {
            return false;
        };
        if !guard.metadata().is_ok_and(|metadata| metadata.is_dir()) {
            return false;
        }
        guards.push(guard);
    }
    true
}

fn write_file(temporary: &Path, bytes: &[u8], mode: WorktreeMode) -> io::Result<()> {
    if mode == WorktreeMode::Symlink {
        write_link(bytes, temporary)?;
    } else {
        fs::write(temporary, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = if mode == WorktreeMode::Executable {
                0o755
            } else {
                0o644
            };
            fs::set_permissions(temporary, fs::Permissions::from_mode(permissions))?;
        }
    }
    Ok(())
}

/// Move the current entry aside before inspecting its version. An atomic
/// editor save before the move is verified, and one after it can create the
/// path without being overwritten by checkout or rollback.
fn install_file(
    source: Option<&Path>,
    target: &Path,
    scratch: precondition::ScratchDir,
    path: &RepoPath,
    expected: Option<&FileVersion>,
    hash: gix::hash::Kind,
) -> Result<(), Failure> {
    let environment = |error: io::Error| Failure {
        class: ErrorClass::Environment,
        details: format!("{}: {error}", path.0),
    };
    let changed = || Failure {
        class: ErrorClass::PreconditionChanged,
        details: format!("{} changed before replacement", path.0),
    };
    let install = |source| {
        rename_into_vacant_path(source, target).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists || fs::symlink_metadata(target).is_ok()
            {
                changed()
            } else {
                environment(error)
            }
        })
    };
    if source.is_some() {
        // The repository path was validated by the caller.
        fs::create_dir_all(target.parent().expect("worktree file has a parent"))
            .map_err(environment)?;
    }
    let Some(expected) = expected else {
        return match source {
            Some(source) => install(source),
            None if leaf(target, path)?.is_none() => Ok(()),
            None => Err(changed()),
        };
    };

    // A separate directory keeps the backup clear of every prepared path.
    let backup = precondition::ScratchDir::create(&scratch.0).map_err(environment)?;
    let aside = backup.0.join("previous");
    rename_into_vacant_path(target, &aside).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            changed()
        } else {
            environment(error)
        }
    })?;
    let result = match version_on_disk(&aside, path, hash) {
        Ok(ref found) if found.as_ref() == Some(expected) => source.map_or(Ok(()), install),
        Ok(_) => Err(changed()),
        Err(failure) => Err(failure),
    };
    if let Err(mut failure) = result {
        if let Err(error) = rename_into_vacant_path(&aside, target) {
            // A newer file may already own the worktree path. Preserve both.
            std::mem::forget(backup);
            std::mem::forget(scratch);
            failure.details.push_str(&format!(
                ". The previous file is kept at {} because it could not be put back: {error}",
                aside.display()
            ));
        }
        return Err(failure);
    }
    Ok(())
}

#[cfg(windows)]
fn rename_into_vacant_path(source: &Path, target: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let source = precondition::verbatim_path(source)?;
    let target = precondition::verbatim_path(target)?;
    // SAFETY: both path buffers are NUL-terminated and live through the call.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_into_vacant_path(source: &Path, target: &Path) -> io::Result<()> {
    use std::ffi::{CString, c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        #[cfg(target_os = "linux")]
        fn renameat2(
            old_dir: c_int,
            old: *const c_char,
            new_dir: c_int,
            new: *const c_char,
            flags: c_uint,
        ) -> c_int;
        #[cfg(target_os = "macos")]
        fn renamex_np(old: *const c_char, new: *const c_char, flags: c_uint) -> c_int;
    }

    let source = CString::new(source.as_os_str().as_bytes())?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: both paths are NUL-terminated and live through the call.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        const AT_FDCWD: c_int = -100;
        const RENAME_NOREPLACE: c_uint = 1;
        renameat2(
            AT_FDCWD,
            source.as_ptr(),
            AT_FDCWD,
            target.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    // SAFETY: both paths are NUL-terminated and live through the call.
    #[cfg(target_os = "macos")]
    let result = unsafe {
        const RENAME_EXCL: c_uint = 4;
        renamex_np(source.as_ptr(), target.as_ptr(), RENAME_EXCL)
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn rename_into_vacant_path(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

fn remove_empty_parents(full: &Path, workdir: &Path) {
    let mut dir = full.parent();
    while let Some(current) = dir {
        if current == workdir || fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
}

fn failed(class: ErrorClass, details: String) -> OpOutcome {
    OpOutcome::Failed(Failure { class, details })
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::os::windows::ffi::OsStrExt;
    use std::process::Command;

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::*;

    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;

    /// Gives `path` a reparse point with a third-party tag that is not a name
    /// surrogate, like a cloud placeholder (OneDrive) or a dedup stub.
    fn set_placeholder_tag(path: &Path) -> io::Result<()> {
        let tag: u32 = 0x0000_7A11;
        let data = [1u8, 2, 3, 4];
        let mut buffer = Vec::new();
        buffer.extend(tag.to_le_bytes());
        buffer.extend((data.len() as u16).to_le_bytes());
        buffer.extend(0u16.to_le_bytes());
        buffer.extend([0x5Au8; 16]);
        buffer.extend(data);
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: a NUL-terminated path, a buffer that outlives the call, and
        // the handle is closed before returning.
        unsafe {
            let handle = CreateFileW(
                wide.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            );
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let mut returned = 0;
            let done = DeviceIoControl(
                handle,
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            );
            let error = io::Error::last_os_error();
            CloseHandle(handle);
            if done == 0 {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Cross review: only name-surrogate reparse points (symlinks,
    /// junctions) are links. A placeholder folder or file is used as is.
    #[test]
    fn placeholder_reparse_points_are_plain_folders_and_files() {
        let root = std::env::temp_dir().join(format!(
            "invoke-git-reparse-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(root.join("cloud")).unwrap();
        fs::create_dir_all(root.join("plain")).unwrap();
        fs::write(root.join("plain/file"), b"x").unwrap();
        fs::create_dir_all(root.join("real")).unwrap();
        // A folder: traversing it needs its filter driver, so only its own
        // entry is looked at here; the file sits in a plain folder.
        let tagged = set_placeholder_tag(&root.join("cloud"))
            .and_then(|()| set_placeholder_tag(&root.join("plain/file")));
        if let Err(error) = &tagged {
            let _ = fs::remove_dir_all(&root);
            panic!("set a placeholder reparse point: {error}");
        }
        let junction = Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(root.join("junction"))
            .arg(root.join("real"))
            .output()
            .unwrap();
        let under_folder = worktree_path(&root, &RepoPath("cloud/file".into()));
        let path = RepoPath("plain/file".into());
        let leaf =
            worktree_path(&root, &path).map(|full| check_leaf(&full, &path).map(|m| m.is_some()));
        let through_junction = worktree_path(&root, &RepoPath("junction/file".into()));
        let _ = fs::remove_dir_all(&root);
        assert!(junction.status.success(), "{junction:?}");
        assert!(under_folder.is_ok(), "{under_folder:?}");
        assert_eq!(leaf, Ok(Ok(true)));
        assert!(through_junction.is_err(), "{through_junction:?}");
    }

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "invoke-git-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn icacls(args: &[&std::ffi::OsStr]) {
        let done = Command::new("icacls").args(args).output().unwrap();
        assert!(done.status.success(), "icacls {args:?}: {done:?}");
    }

    fn set_read_only(path: &Path, on: bool) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(on);
        fs::set_permissions(path, permissions).unwrap();
    }

    /// Re-check: a rename that fails for a reason other than the target
    /// (here the source may not be moved) must leave the user's file where
    /// it was, writable or read-only.
    #[test]
    fn a_failed_install_never_loses_the_target() {
        let root = temp_root("install");
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        let source = locked.join("source");
        fs::write(&source, b"new").unwrap();
        // Everyone may not delete the source or anything in its folder, so
        // it cannot be renamed away.
        let everyone = std::ffi::OsStr::new("*S-1-1-0:(DE)");
        let children = std::ffi::OsStr::new("*S-1-1-0:(DC)");
        let deny = std::ffi::OsStr::new("/deny");
        icacls(&[source.as_os_str(), deny, everyone]);
        icacls(&[locked.as_os_str(), deny, children]);
        let mut outcomes = Vec::new();
        for read_only in [false, true] {
            let target = root.join(format!("target-{read_only}"));
            fs::write(&target, b"user's").unwrap();
            if read_only {
                set_read_only(&target, true);
            }
            let scratch = precondition::ScratchDir::create(&root).unwrap();
            let path = RepoPath(format!("target-{read_only}"));
            let hash = gix::hash::Kind::Sha1;
            let expected = file_version(&target, &path, hash).unwrap();
            let result = install_file(
                Some(&source),
                &target,
                scratch,
                &path,
                expected.as_ref(),
                hash,
            );
            outcomes.push((
                read_only,
                result,
                fs::read(&target).ok(),
                fs::metadata(&target)
                    .map(|m| m.permissions().readonly())
                    .ok(),
            ));
            if read_only && target.exists() {
                set_read_only(&target, false);
            }
        }
        let remove = std::ffi::OsStr::new("/remove:d");
        let sid = std::ffi::OsStr::new("*S-1-1-0");
        icacls(&[locked.as_os_str(), remove, sid]);
        icacls(&[source.as_os_str(), remove, sid]);
        let _ = fs::remove_dir_all(&root);
        for (read_only, result, bytes, kept_read_only) in outcomes {
            assert!(result.is_err(), "read-only {read_only}: {result:?}");
            assert_eq!(
                bytes.as_deref(),
                Some(&b"user's"[..]),
                "read-only {read_only}"
            );
            assert_eq!(kept_read_only, Some(read_only));
        }
    }

    /// The case the fallback exists for: a read-only target is replaced,
    /// and the copy moved aside goes with the scratch folder.
    #[test]
    fn a_read_only_target_is_replaced_and_nothing_is_left_behind() {
        let root = temp_root("install-ro");
        let target = root.join("target");
        fs::write(&target, b"user's").unwrap();
        set_read_only(&target, true);
        let scratch = precondition::ScratchDir::create(&root).unwrap();
        let source = scratch.0.join("previous");
        fs::write(&source, b"new").unwrap();
        let path = RepoPath("target".into());
        let hash = gix::hash::Kind::Sha1;
        let expected = file_version(&target, &path, hash).unwrap();
        let scratch_path = scratch.0.clone();
        let result = install_file(
            Some(&source),
            &target,
            scratch,
            &path,
            expected.as_ref(),
            hash,
        );
        let bytes = fs::read(&target).ok();
        let left = scratch_path.exists();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(result, Ok(()));
        assert_eq!(bytes.as_deref(), Some(&b"new"[..]));
        assert!(!left, "the scratch folder was left behind");
    }

    #[test]
    fn late_atomic_editor_save_survives_replacement_and_deletion() {
        let root = temp_root("editor-save");
        let target = root.join("target");
        let path = RepoPath("target".into());
        let hash = gix::hash::Kind::Sha1;
        for replace in [true, false] {
            let scratch = precondition::ScratchDir::create(&root).unwrap();
            fs::write(&target, b"confirmed").unwrap();
            let expected = file_version(&target, &path, hash).unwrap();
            let editor = root.join("editor.tmp");
            fs::write(&editor, b"new edit").unwrap();
            fs::rename(&editor, &target).unwrap();
            let source = scratch.0.join("checkout");
            fs::write(&source, b"checkout").unwrap();
            let failure = install_file(
                replace.then_some(source.as_path()),
                &target,
                scratch,
                &path,
                expected.as_ref(),
                hash,
            )
            .unwrap_err();
            assert_eq!(failure.class, ErrorClass::PreconditionChanged);
            assert_eq!(
                fs::read(&target).ok().as_deref(),
                Some(&b"new edit"[..]),
                "replace={replace}, expected {expected:?}"
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stage_guard_pins_the_file_and_its_parent_until_git_finishes() {
        let root = precondition::ScratchDir::create(&std::env::temp_dir()).unwrap();
        let dir = root.0.join("dir");
        let target = dir.join("file");
        fs::create_dir(&dir).unwrap();
        fs::write(&target, b"staged").unwrap();
        let (guards, missing) = stage_path_guard(&root.0, &RepoPath("dir/file".into())).unwrap();
        assert!(missing.is_none());
        assert!(fs::rename(&dir, root.0.join("moved")).is_err());
        assert!(fs::rename(&target, dir.join("moved")).is_err());
        drop(guards);
        fs::rename(&dir, root.0.join("moved")).unwrap();
    }

    /// Re-check: the link kind is decided only for relative targets that
    /// stay inside the worktree through real folders. Anything else (an
    /// absolute or UNC path, `..` out of the repo, a junction) keeps Git's
    /// default, a file link, without looking outside.
    #[test]
    fn a_link_is_a_directory_link_only_for_a_folder_inside_the_worktree() {
        let root = temp_root("link-kind");
        let workdir = root.join("repo");
        fs::create_dir_all(workdir.join("dir")).unwrap();
        fs::create_dir_all(workdir.join("a")).unwrap();
        fs::write(workdir.join("file"), b"x").unwrap();
        fs::create_dir_all(root.join("outside")).unwrap();
        let junction = Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(workdir.join("junction"))
            .arg(root.join("outside"))
            .output()
            .unwrap();
        let outside = root.join("outside");
        let cases: Vec<(&str, PathBuf, bool)> = vec![
            ("link", PathBuf::from("dir"), true),
            ("a/link", PathBuf::from("../dir"), true),
            ("link", PathBuf::from("a/../dir"), true),
            ("link", PathBuf::from("file"), false),
            ("link", PathBuf::from("missing"), false),
            ("link", PathBuf::from("../outside"), false),
            ("a/link", PathBuf::from("../../outside"), false),
            ("link", outside.clone(), false),
            ("link", PathBuf::from(r"\\localhost\C$"), false),
            ("link", PathBuf::from(r"\outside"), false),
            ("link", PathBuf::from("junction"), false),
            ("link", PathBuf::from("dir/C:relative"), false),
            ("link", PathBuf::from("junction./folder"), false),
        ];
        let found: Vec<bool> = cases
            .iter()
            .map(|(link, target, _)| wants_dir_link(&workdir, &RepoPath((*link).into()), target))
            .collect();
        let _ = fs::remove_dir_all(&root);
        assert!(junction.status.success(), "{junction:?}");
        for ((link, target, want), found) in cases.iter().zip(found) {
            assert_eq!(found, *want, "{link} -> {}", target.display());
        }
    }
}
