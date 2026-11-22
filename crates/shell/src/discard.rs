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
use std::path::{Component, Path, PathBuf};

use invoke_core::{
    DiscardRestore, ErrorClass, Event, Failure, IndexId, Oid, OpId, OpOutcome, Patch, RepoPath,
};

use crate::git::{GLOBAL_ARGS, Git};
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
        match file_blob(&full, path, hash) {
            Ok(found) if found.as_ref() == expect_file => {}
            Ok(found) => {
                return stale(format!(
                    "{} changed since the diff was made: expected {expect_file:?}, found {found:?}",
                    path.0
                ));
            }
            Err(details) => return failed(ErrorClass::Environment, details),
        }
        // What the snapshot saved must be what is about to be discarded, or
        // an undo would bring back something else.
        match self.undo.read_saved(&discard_ref(op)) {
            Ok(saved) if saved.as_ref().map(|file| &file.blob) == expect_file => {}
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

/// One file in a saved tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SavedFile {
    pub path: String,
    /// `100644` or `100755`.
    pub mode: String,
    pub blob: Oid,
}

/// The file as it is on disk, or `None` when it does not exist.
pub(crate) struct OnDisk {
    pub bytes: Vec<u8>,
    pub executable: bool,
}

/// `path` under `workdir`, refusing anything that could leave the repo or
/// reach `.git`: a discard (and its undo) writes and deletes files.
pub(crate) fn worktree_path(workdir: &Path, path: &RepoPath) -> Result<PathBuf, String> {
    let relative = Path::new(&path.0);
    let safe = !path.0.is_empty()
        && relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && !matches!(
            relative.components().next(),
            Some(Component::Normal(first)) if first.eq_ignore_ascii_case(".git")
        );
    if safe {
        Ok(workdir.join(relative))
    } else {
        Err(format!(
            "{:?} is not a path inside the working tree",
            path.0
        ))
    }
}

/// Reads `full`; `None` when absent, an error for a directory, a symlink
/// or anything else that is not a plain file.
pub(crate) fn read_file(full: &Path, path: &RepoPath) -> Result<Option<OnDisk>, String> {
    let metadata = match fs::symlink_metadata(full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.0)),
    };
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.0));
    }
    let bytes = fs::read(full).map_err(|error| format!("{}: {error}", path.0))?;
    Ok(Some(OnDisk {
        bytes,
        executable: is_executable(&metadata),
    }))
}

/// The blob id of the file's exact bytes, `None` when it does not exist.
fn file_blob(full: &Path, path: &RepoPath, hash: gix::hash::Kind) -> Result<Option<Oid>, String> {
    match read_file(full, path)? {
        Some(file) => blob_id(hash, &file.bytes).map(Some),
        None => Ok(None),
    }
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

/// Windows has no executable bit; git records files as `100644` there.
#[cfg(not(unix))]
fn is_executable(_: &fs::Metadata) -> bool {
    false
}

impl UndoRunner {
    /// Saves the worktree file `path` as a tree under `ref_name` (the file
    /// as a blob with its mode; an empty tree when the file does not
    /// exist). The bytes are stored as they are, without any filter.
    pub(crate) fn save_file(&self, ref_name: &str, path: &RepoPath) -> Result<(), String> {
        let full = worktree_path(&self.paths.workdir, path)?;
        let file = read_file(&full, path)?;
        // A private index file, so the real one is never touched. A name
        // per ref keeps a snapshot and an after-record from colliding.
        let tag = ref_name.replace('/', "-");
        let private = self
            .paths
            .git_dir
            .join(format!("invoke-undo-index-save-{tag}"));
        let _ = fs::remove_file(&private);
        let env = [("GIT_INDEX_FILE", private.as_os_str())];
        let saved = (|| {
            if let Some(file) = &file {
                let blob = self.run(
                    &["hash-object", "-w", "--no-filters", "--stdin"],
                    &[],
                    Some(&file.bytes),
                )?;
                let mode = if file.executable { "100755" } else { "100644" };
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
            self.run(&["update-ref", ref_name, tree.trim()], &[], None)
        })();
        let _ = fs::remove_file(&private);
        saved.map(|_| ())
    }

    /// The file saved under `ref_name`: `None` for an empty tree (the file
    /// did not exist).
    pub(crate) fn read_saved(&self, ref_name: &str) -> Result<Option<SavedFile>, String> {
        let listing = self.run(&["ls-tree", "-r", "-z", ref_name], &[], None)?;
        let Some(entry) = listing.split('\0').find(|entry| !entry.is_empty()) else {
            return Ok(None);
        };
        // `<mode> SP <type> SP <oid> TAB <path>`
        let parse = || -> Option<SavedFile> {
            let (meta, path) = entry.split_once('\t')?;
            let mut parts = meta.split(' ');
            let mode = parts.next()?.to_owned();
            let _kind = parts.next()?;
            let blob = Oid::from_hex(parts.next()?).ok()?;
            Some(SavedFile {
                path: path.to_owned(),
                mode,
                blob,
            })
        };
        parse()
            .map(Some)
            .ok_or_else(|| format!("cannot read the saved file entry {entry:?}"))
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
            self.read_saved(&discard_ref(restore.discarded)),
            self.read_saved(&discard_after_ref(restore.discarded)),
        ) {
            (Ok(saved), Ok(after)) => (saved, after),
            (Err(details), _) | (_, Err(details)) => {
                return failed(
                    ErrorClass::Bug,
                    format!("the saved copy of the discarded file is gone: {details}"),
                );
            }
        };
        match file_blob(&full, path, repo.object_hash()) {
            Ok(now) if now == after.map(|file| file.blob) => {}
            Ok(_) => {
                return stale(format!(
                    "{} was changed after the discard; undo does not overwrite it",
                    path.0
                ));
            }
            Err(details) => return stale(details),
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
                if let Err(error) = write_file(&full, &object.data, file.mode == "100755") {
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

/// Writes `bytes` to `full` through a temporary file in the same folder, so
/// the old file is replaced in one step and never left half written.
fn write_file(full: &Path, bytes: &[u8], executable: bool) -> io::Result<()> {
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temporary = full.as_os_str().to_owned();
    temporary.push(".invoke-undo-tmp");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    fs::rename(&temporary, full).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })
}

/// Removes the file, then the folders it leaves empty (up to the repo).
fn remove_file_and_empty_dirs(full: &Path, workdir: &Path) -> io::Result<()> {
    match fs::remove_file(full) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
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
