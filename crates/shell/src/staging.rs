//! The system-git half of staging (backlog task 11): `ApplyPatchToIndex`,
//! and the file contents a diff is made from.
//!
//! The patch itself is built in the pure `diff` crate; nothing here
//! decides what to stage. [`StagingRunner::apply`] refuses when disk moved
//! (rule 5), checks the patch with `git apply --cached --check`, and only
//! then applies it. [`load_inputs`] reads the three sides of a file (`HEAD`,
//! the index, the worktree) through gix and plain file reads.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use invoke_core::{ErrorClass, Event, Failure, IndexId, Oid, OpId, OpOutcome, Patch, RepoPath};

use crate::git::{GLOBAL_ARGS, Git};
use crate::precondition::{self, RepoPaths};
use crate::runner::{Control, run_child};

pub struct StagingRunner {
    git: Git,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
}

impl StagingRunner {
    pub fn new(git: Git, paths: RepoPaths) -> Self {
        Self {
            git,
            paths,
            env: Vec::new(),
        }
    }

    /// Extra environment for every git run (tests use it to isolate config).
    pub fn with_env(mut self, env: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        self.env.extend(env);
        self
    }

    /// Runs `Command::ApplyPatchToIndex`: the `OpFinished` for `op`.
    ///
    /// The worktree is never touched: `--cached` only writes the index.
    pub fn apply(&self, op: OpId, patch: &Patch, expect_index: &IndexId) -> Event {
        Event::OpFinished(op, self.apply_outcome(patch, expect_index))
    }

    fn apply_outcome(&self, patch: &Patch, expect_index: &IndexId) -> OpOutcome {
        let stale = |details: String| {
            OpOutcome::Failed(Failure {
                class: ErrorClass::PreconditionChanged,
                details,
            })
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
            Err(details) => {
                return OpOutcome::Failed(Failure {
                    class: ErrorClass::Bug,
                    details,
                });
            }
        }
        // Same flags both times, as `git add -p` does. `nowarn` keeps CRLF
        // lines from producing whitespace warnings (or errors, when the
        // user's config says `apply.whitespace=error`).
        for check in [true, false] {
            let mut command = self.git.command(&self.paths.workdir);
            command
                .args(GLOBAL_ARGS)
                .args(["apply", "--cached", "--whitespace=nowarn"])
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
                    return OpOutcome::Failed(Failure {
                        class: ErrorClass::Environment,
                        details: format!("start git: {error}"),
                    });
                }
            };
            if !done.success {
                // The index id matched, so a patch that does not apply is
                // one we built wrong, or a lock taken a moment ago.
                let class = if done.output.contains("index.lock") {
                    ErrorClass::PreconditionChanged
                } else {
                    ErrorClass::Bug
                };
                return OpOutcome::Failed(Failure {
                    class,
                    details: done.output,
                });
            }
        }
        OpOutcome::Succeeded
    }
}

/// Which two sides a diff compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Index to worktree: what staging works on.
    IndexToWorktree,
    /// `HEAD` to index: what unstaging works on.
    HeadToIndex,
}

/// The two texts of a file diff, as read now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffInputs {
    /// `None`: the file does not exist on that side.
    pub old: Option<String>,
    pub new: Option<String>,
    /// The executable bit of the side that exists (the new side if both).
    pub executable: bool,
    /// The blob id of the new side's bytes, exactly as they are (no
    /// filters), or `None` when the new side does not exist. A discard
    /// carries it as `expect_file` (task 13).
    pub new_blob: Option<Oid>,
    /// The index id the sides were read under; a stage built from them
    /// carries it as `based_on`.
    pub based_on: IndexId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputsError {
    /// Binary, not UTF-8, a symlink, a submodule or an intent-to-add entry:
    /// no text patch can stage it (a later task adds `git add` for these).
    NotText(String),
    /// The index moved while it was read; read again.
    Moved,
    Read(String),
}

/// Reads both sides of `path` for [`Source`].
pub fn load_inputs(
    paths: &RepoPaths,
    path: &RepoPath,
    source: Source,
) -> Result<DiffInputs, InputsError> {
    let read = |error: String| InputsError::Read(error);
    let repo = precondition::open_with_worktree(paths).map_err(read)?;
    let hash_len = repo.object_hash().len_in_bytes();
    let index_path = paths.git_dir.join("index");
    let id = |at: &Path| {
        precondition::index_id(at, hash_len).map_err(|e| InputsError::Read(format!("index: {e}")))
    };
    let before = id(&index_path)?;

    let index_side = read_index_side(&repo, path)?;
    let (old, new) = match source {
        Source::IndexToWorktree => (index_side.clone(), read_worktree_side(&repo, paths, path)?),
        Source::HeadToIndex => (read_head_side(&repo, path)?, index_side.clone()),
    };

    if id(&index_path)? != before {
        return Err(InputsError::Moved);
    }
    let executable = new
        .as_ref()
        .or(old.as_ref())
        .is_some_and(|side| side.executable);
    let new_blob = new.as_ref().map(|side| side.blob.clone());
    Ok(DiffInputs {
        old: old.map(|side| side.text),
        new: new.map(|side| side.text),
        new_blob,
        executable,
        based_on: before,
    })
}

#[derive(Clone)]
struct Side {
    text: String,
    executable: bool,
    /// The blob id of the bytes.
    blob: Oid,
}

fn text_of(
    path: &RepoPath,
    bytes: Vec<u8>,
    executable: bool,
    blob: Oid,
) -> Result<Side, InputsError> {
    if bytes.iter().take(8000).any(|b| *b == 0) {
        return Err(InputsError::NotText(format!("{} is binary", path.0)));
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(Side {
            text,
            executable,
            blob,
        }),
        Err(_) => Err(InputsError::NotText(format!("{} is not UTF-8", path.0))),
    }
}

fn read_index_side(repo: &gix::Repository, path: &RepoPath) -> Result<Option<Side>, InputsError> {
    use gix::index::entry::{Flags, Mode};
    let read = |e: String| InputsError::Read(e);
    let index = repo
        .index_or_empty()
        .map_err(|e| read(format!("read index: {e}")))?;
    let Some(entry) = index.entry_by_path(path.0.as_str().into()) else {
        return Ok(None);
    };
    if entry.flags.contains(Flags::INTENT_TO_ADD) {
        return Err(InputsError::NotText(format!("{} is intent-to-add", path.0)));
    }
    let executable = match entry.mode {
        Mode::FILE => false,
        Mode::FILE_EXECUTABLE => true,
        _ => {
            return Err(InputsError::NotText(format!(
                "{} is not a regular file",
                path.0
            )));
        }
    };
    let blob = repo
        .find_object(entry.id)
        .map_err(|e| read(format!("read blob: {e}")))?;
    let id = precondition::oid(&entry.id).map_err(read)?;
    text_of(path, blob.data.clone(), executable, id).map(Some)
}

fn read_head_side(repo: &gix::Repository, path: &RepoPath) -> Result<Option<Side>, InputsError> {
    use gix::object::tree::EntryKind;
    let read = |e: String| InputsError::Read(e);
    let Ok(commit) = repo.head_commit() else {
        return Ok(None); // unborn: nothing in HEAD
    };
    let tree = commit.tree().map_err(|e| read(format!("read tree: {e}")))?;
    let Some(entry) = tree
        .lookup_entry_by_path(path.0.as_str())
        .map_err(|e| read(format!("look up {}: {e}", path.0)))?
    else {
        return Ok(None);
    };
    let executable = match entry.mode().kind() {
        EntryKind::Blob => false,
        EntryKind::BlobExecutable => true,
        _ => {
            return Err(InputsError::NotText(format!(
                "{} is not a regular file",
                path.0
            )));
        }
    };
    let object = entry
        .object()
        .map_err(|e| read(format!("read blob: {e}")))?;
    let id = precondition::oid(&object.id).map_err(read)?;
    text_of(path, object.data.clone(), executable, id).map(Some)
}

/// The blob id of `bytes`, as the system tool's `hash-object --no-filters`
/// gives it.
pub fn blob_id(hash: gix::hash::Kind, bytes: &[u8]) -> Result<Oid, String> {
    let id = gix::objs::compute_hash(hash, gix::object::Kind::Blob, bytes)
        .map_err(|error| format!("hash: {error}"))?;
    precondition::oid(&id)
}

/// The worktree file's text as `git add` would store it: through the repo's
/// filters (`core.autocrlf`, `.gitattributes` `text` and `eol`), so a CRLF
/// checkout of an LF blob is the same text as the index and only real edits
/// differ. The blob id is that of the raw file bytes.
fn read_worktree_side(
    repo: &gix::Repository,
    paths: &RepoPaths,
    path: &RepoPath,
) -> Result<Option<Side>, InputsError> {
    let full = paths.workdir.join(&path.0);
    let metadata = match fs::symlink_metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(InputsError::Read(format!("{}: {error}", path.0))),
    };
    if !metadata.is_file() {
        return Err(InputsError::NotText(format!(
            "{} is not a regular file",
            path.0
        )));
    }
    let raw = fs::read(&full).map_err(|error| InputsError::Read(format!("{}: {error}", path.0)))?;
    // The id is of the file's own bytes: discard and its undo compare those
    // exactly. Only the text for the diff goes through the filters.
    let blob = blob_id(repo.object_hash(), &raw).map_err(InputsError::Read)?;
    let bytes = filter_to_git(repo, path, raw)?;
    text_of(path, bytes, is_executable(&metadata), blob).map(Some)
}

/// Runs `bytes` through the repo's to-git filters, as `git add` does.
fn filter_to_git(
    repo: &gix::Repository,
    path: &RepoPath,
    bytes: Vec<u8>,
) -> Result<Vec<u8>, InputsError> {
    use std::io::Read;
    let read = |what: &str, error: &dyn std::fmt::Display| {
        InputsError::Read(format!("{what} {}: {error}", path.0))
    };
    let (mut pipeline, index) = repo
        .filter_pipeline(None)
        .map_err(|e| read("prepare filters for", &e))?;
    let state: &gix::index::State = match &index {
        gix::worktree::IndexPersistedOrInMemory::Persisted(file) => file,
        gix::worktree::IndexPersistedOrInMemory::InMemory(file) => file,
    };
    let mut outcome = pipeline
        .convert_to_git(&bytes[..], Path::new(&path.0), state)
        .map_err(|e| read("filter", &e))?;
    let mut converted = Vec::with_capacity(bytes.len());
    outcome
        .read_to_end(&mut converted)
        .map_err(|e| read("read filtered", &e))?;
    Ok(converted)
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
