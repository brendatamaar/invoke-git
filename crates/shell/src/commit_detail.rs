//! What one commit changed (task 19): its message, parents, and the files
//! it touched against its first parent, read through gix. A root commit is
//! compared with the empty tree. A merge is shown against its first parent
//! only, the way `git log -m --first-parent` does, and says so.

use invoke_core::RepoPath;

use crate::history::CommitId;
use crate::precondition::{self, RepoPaths};
use crate::staging::{InputsError, text_of};

/// What happened to a path in the commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Added,
    Modified,
    Deleted,
    /// A file became a symlink or the other way round, or a submodule.
    TypeChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    pub change: Change,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitDetail {
    pub id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    /// Author time and the author's UTC offset, in seconds.
    pub seconds: i64,
    pub offset: i32,
    /// The whole message, line endings as stored.
    pub message: String,
    /// Against the first parent (the empty tree for a root commit), sorted
    /// by path.
    pub files: Vec<ChangedFile>,
}

impl CommitDetail {
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// The two sides of one file of a commit. `None`: the file does not exist
/// on that side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitSides {
    pub old: Option<String>,
    pub new: Option<String>,
}

pub(crate) fn tree_of<'r>(commit: &gix::Commit<'r>) -> Result<gix::Tree<'r>, String> {
    commit.tree().map_err(|e| format!("read tree: {e}"))
}

/// The first parent's tree, or the empty tree for a root commit.
fn parent_tree<'r>(
    repo: &'r gix::Repository,
    commit: &gix::Commit<'r>,
) -> Result<gix::Tree<'r>, String> {
    match commit.parent_ids().next() {
        Some(parent) => {
            let parent = repo
                .find_commit(parent)
                .map_err(|e| format!("read parent: {e}"))?;
            tree_of(&parent)
        }
        None => Ok(repo.empty_tree()),
    }
}

/// The files that differ between two trees, sorted by path. Directories are
/// listed through the files inside them; renames are not detected.
pub(crate) fn changed_files(
    repo: &gix::Repository,
    old_tree: &gix::Tree<'_>,
    new_tree: &gix::Tree<'_>,
) -> Result<Vec<ChangedFile>, String> {
    use gix::object::tree::diff::ChangeDetached as C;
    let changes = repo
        .diff_tree_to_tree(
            old_tree,
            new_tree,
            gix::diff::Options::default().with_rewrites(None),
        )
        .map_err(|e| format!("diff trees: {e}"))?;
    let mut files: Vec<ChangedFile> = changes
        .into_iter()
        .filter_map(|change| {
            let (location, change, mode) = match change {
                C::Addition {
                    location,
                    entry_mode,
                    ..
                } => (location, Change::Added, entry_mode),
                C::Deletion {
                    location,
                    entry_mode,
                    ..
                } => (location, Change::Deleted, entry_mode),
                C::Modification {
                    location,
                    previous_entry_mode,
                    entry_mode,
                    ..
                } => {
                    let kind = if previous_entry_mode.kind() == entry_mode.kind()
                        || (previous_entry_mode.is_blob() && entry_mode.is_blob())
                    {
                        Change::Modified
                    } else {
                        Change::TypeChanged
                    };
                    (location, kind, entry_mode)
                }
                C::Rewrite { .. } => return None,
            };
            // Directories are listed through the files inside them.
            if mode.is_tree() {
                return None;
            }
            Some(ChangedFile {
                path: String::from_utf8_lossy(&location).into_owned(),
                change,
            })
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

pub fn read_commit(paths: &RepoPaths, id: &CommitId) -> Result<CommitDetail, String> {
    let repo = precondition::open(paths)?;
    let commit = repo
        .find_commit(*id)
        .map_err(|e| format!("read commit {id}: {e}"))?;
    let new_tree = tree_of(&commit)?;
    let old_tree = parent_tree(&repo, &commit)?;
    let files = changed_files(&repo, &old_tree, &new_tree)?;
    let author = commit.author().map_err(|e| format!("read author: {e}"))?;
    let time = author.time().map_err(|e| format!("read time: {e}"))?;
    Ok(CommitDetail {
        id: id.to_string(),
        parents: commit.parent_ids().map(|p| p.to_string()).collect(),
        author: String::from_utf8_lossy(author.name).into_owned(),
        email: String::from_utf8_lossy(author.email).into_owned(),
        seconds: time.seconds,
        offset: time.offset,
        message: String::from_utf8_lossy(commit.message_raw_sloppy()).into_owned(),
        files,
    })
}

/// Reads the text of `path` in `id` and in its first parent. A side that is
/// binary, not UTF-8, a symlink or a submodule is an error with the reason
/// (there is no text diff to show).
pub fn read_commit_sides(
    paths: &RepoPaths,
    id: &CommitId,
    path: &RepoPath,
) -> Result<CommitSides, InputsError> {
    let read = InputsError::Read;
    let repo = precondition::open(paths).map_err(read)?;
    let commit = repo
        .find_commit(*id)
        .map_err(|e| InputsError::Read(format!("read commit {id}: {e}")))?;
    let new_tree = tree_of(&commit).map_err(InputsError::Read)?;
    let old_tree = parent_tree(&repo, &commit).map_err(InputsError::Read)?;
    Ok(CommitSides {
        old: side(&old_tree, path)?,
        new: side(&new_tree, path)?,
    })
}

pub(crate) fn side(tree: &gix::Tree<'_>, path: &RepoPath) -> Result<Option<String>, InputsError> {
    use gix::object::tree::EntryKind;
    let read = |e: String| InputsError::Read(e);
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
    let blob = precondition::oid(&object.id).map_err(read)?;
    text_of(path, object.data.clone(), executable, blob).map(|side| Some(side.text))
}
