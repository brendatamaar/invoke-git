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
            .then(|| scratch_for(&full))
            .transpose()
            .map_err(environment)?;
        let staged = if let Some(scratch) = &scratch {
            let mut prefix = OsString::from("--prefix=");
            prefix.push(&scratch.0);
            prefix.push("/");
            let mut run = invocation(args);
            run.args.insert(GLOBAL_ARGS.len() + 1, prefix);
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
            Some(scratch.0.join(&path.0))
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
        lock.verify(&self.paths, expect)?;
        if now()?.as_ref() != file {
            return Err(changed());
        }
        if let Some(staged) = staged {
            install_file(&staged, &full).map_err(environment)?;
        } else {
            remove_leaf(&full).map_err(environment)?;
        }
        Ok(())
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
        let full = match worktree_path(&self.paths.workdir, path) {
            Ok(full) => full,
            Err(details) => return failed(ErrorClass::Bug, details),
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
        let before = match file_version(&full, path, hash) {
            Ok(Some(file)) if file.mode == WorktreeMode::Symlink => {
                return stale(format!("{} is now a symlink", path.0));
            }
            Ok(before) => before,
            Err(details) => return failed(ErrorClass::Environment, details),
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
        if let Err(details) = self.undo.save_file(&discard_after_ref(op), path) {
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
    let tracked = checkout_entry(&repo, path)?;
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

fn checkout_entry(repo: &gix::Repository, path: &RepoPath) -> Result<bool, String> {
    Ok(index_entry(repo, path)?.is_some_and(|entry| {
        !entry
            .flags
            .contains(gix::index::entry::Flags::INTENT_TO_ADD)
    }))
}

/// `path` under `workdir`, refusing anything that could leave the repo or
/// reach `.git`: a discard (and its undo) writes and deletes files.
pub(crate) fn worktree_path(workdir: &Path, path: &RepoPath) -> Result<PathBuf, String> {
    let invalid = || format!("{:?} is not a path inside the working tree", path.0);
    let mut full = workdir.to_path_buf();
    let mut parts = path.0.split('/').peekable();
    while let Some(part) = parts.next() {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.eq_ignore_ascii_case(".git")
            || part.contains('\0')
            || cfg!(windows)
                && (part.contains(['\\', ':'])
                    || part.ends_with(['.', ' '])
                    || part.eq_ignore_ascii_case("git~1"))
        {
            return Err(invalid());
        }
        full.push(part);
        if parts.peek().is_some() {
            match fs::symlink_metadata(&full) {
                Ok(metadata) if metadata.is_dir() && !is_link(&metadata) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(invalid()),
            }
        }
    }
    Ok(full)
}

/// A leaf may be a file, a symlink or absent. Directory pathspecs would
/// affect multiple paths and cannot use a single-file undo.
pub(crate) fn check_leaf(full: &Path, path: &RepoPath) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(full) {
        Ok(metadata)
            if metadata.file_type().is_symlink() || metadata.is_file() && !is_link(&metadata) =>
        {
            Ok(Some(metadata))
        }
        Ok(_) => Err(format!("{} is not a file or symlink", path.0)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.0)),
    }
}

pub(crate) fn read_file(full: &Path, path: &RepoPath) -> Result<Option<OnDisk>, String> {
    let Some(metadata) = check_leaf(full, path)? else {
        return Ok(None);
    };
    let error = |error: io::Error| format!("{}: {error}", path.0);
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
    read_file(full, path)?
        .map(|file| {
            Ok(FileVersion {
                blob: blob_id(hash, &file.bytes)?,
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
fn is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_link(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
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
    pub(crate) fn save_file(&self, ref_name: &str, path: &RepoPath) -> Result<(), String> {
        let full = worktree_path(&self.paths.workdir, path)?;
        let file = read_file(&full, path)?;
        self.save_entry(ref_name, path, file.as_ref())
    }

    fn save_entry(
        &self,
        ref_name: &str,
        path: &RepoPath,
        file: Option<&OnDisk>,
    ) -> Result<(), String> {
        let scratch = precondition::ScratchDir::create(&self.paths.git_dir)
            .map_err(|error| format!("create snapshot directory: {error}"))?;
        let private = scratch.0.join("index");
        let env = [("GIT_INDEX_FILE", private.as_os_str())];
        if let Some(file) = file {
            let blob = self.run(
                &["hash-object", "-w", "--no-filters", "--stdin"],
                &[],
                Some(&file.bytes),
            )?;
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
    /// holds what the discard left. All checks come before the first write.
    pub(crate) fn restore_discard(&self, restore: &DiscardRestore) -> OpOutcome {
        let stale = |details: String| failed(ErrorClass::PreconditionChanged, details);
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
        let check = || -> Result<(), String> {
            worktree_path(&self.paths.workdir, path)?;
            if file_version(&full, path, repo.object_hash())? != after {
                return Err(format!(
                    "{} changed after the discard. Undo requires the discarded version.",
                    path.0
                ));
            }
            Ok(())
        };
        if let Err(details) = check() {
            return stale(details);
        }
        let environment =
            |error: io::Error| failed(ErrorClass::Environment, format!("{}: {error}", path.0));
        match saved {
            Some(file) => {
                let object = match gix::ObjectId::from_hex(file.blob.as_hex().as_bytes())
                    .map_err(|error| error.to_string())
                    .and_then(|id| repo.find_object(id).map_err(|error| error.to_string()))
                {
                    Ok(object) => object,
                    Err(details) => return failed(ErrorClass::Bug, details),
                };
                let scratch = match scratch_for(&full) {
                    Ok(scratch) => scratch,
                    Err(error) => return environment(error),
                };
                let temporary = scratch.0.join("file");
                if let Err(error) = write_file(&temporary, &object.data, file.mode) {
                    return environment(error);
                }
                if let Err(details) = check() {
                    return stale(details);
                }
                if let Err(error) = install_file(&temporary, &full) {
                    return environment(error);
                }
            }
            None => {
                if let Err(error) = remove_file_and_empty_dirs(&full, &self.paths.workdir) {
                    return environment(error);
                }
            }
        }
        OpOutcome::Succeeded
    }
}

/// Choose scratch space on the same filesystem as the eventual replacement.
fn scratch_for(full: &Path) -> io::Result<precondition::ScratchDir> {
    let parent = full.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let existing = parent
        .ancestors()
        .find(|dir| dir.is_dir())
        .ok_or(io::ErrorKind::NotFound)?;
    precondition::ScratchDir::create(existing)
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

fn install_file(source: &Path, target: &Path) -> io::Result<()> {
    let parent = target.parent().ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(parent)?;
    fs::rename(source, target)
}

fn remove_leaf(full: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if fs::symlink_metadata(full).is_ok_and(|m| m.file_type().is_symlink_dir()) {
            return fs::remove_dir(full);
        }
    }
    match fs::remove_file(full) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Removes the file, then the folders it leaves empty (up to the repo).
fn remove_file_and_empty_dirs(full: &Path, workdir: &Path) -> io::Result<()> {
    remove_leaf(full)?;
    let mut dir = full.parent();
    while let Some(current) = dir {
        if current == workdir || fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
    Ok(())
}

fn failed(class: ErrorClass, details: String) -> OpOutcome {
    OpOutcome::Failed(Failure { class, details })
}
