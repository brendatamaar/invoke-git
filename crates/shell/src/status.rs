//! The status tree (backlog task 11): index against `HEAD`, worktree
//! against the index, and untracked files that ignore rules do not hide.
//!
//! Read through gix only, never the git CLI. gix does not take
//! `index.lock` and, unlike `git status`, is not asked to write refreshed
//! stat data back to the index, so a read never changes `.git` (rule 4,
//! law 11).

use std::collections::BTreeMap;

use gix::bstr::ByteSlice;
use gix::status::index_worktree::Item as WorktreeItem;
use gix::status::plumbing::index_as_worktree::{Change as WorktreeChange, EntryStatus};
use gix::status::{Item, UntrackedFiles};
use invoke_core::{FileChange, RepoPath, StatusEntry, StatusSnapshot};

/// Reads the whole status as a sorted list.
///
/// Renames are not detected: a rename shows as a deleted and an added path
/// (`FileChange::Renamed` stays unused until a later task asks for it).
/// Conflicted paths are left out; they are the phase's `Conflicts`.
pub(crate) fn read_status(repo: &gix::Repository) -> Result<StatusSnapshot, String> {
    let fail = |what: &str, error: &dyn std::fmt::Display| format!("{what}: {error}");
    let platform = repo
        .status(gix::progress::Discard)
        .map_err(|e| fail("start status", &e))?
        .untracked_files(UntrackedFiles::Files)
        .index_worktree_rewrites(None)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled);
    let items = platform
        .into_iter(Vec::<gix::bstr::BString>::new())
        .map_err(|e| fail("start status", &e))?;

    let mut staged: BTreeMap<RepoPath, FileChange> = BTreeMap::new();
    let mut unstaged: BTreeMap<RepoPath, FileChange> = BTreeMap::new();
    for item in items {
        match item.map_err(|e| fail("read status", &e))? {
            Item::TreeIndex(change) => {
                if let Some((path, change)) = staged_change(&change) {
                    staged.insert(path, change);
                }
            }
            Item::IndexWorktree(item) => {
                if let Some((path, change)) = worktree_change(&item) {
                    unstaged.insert(path, change);
                }
            }
        }
    }

    let mut entries: BTreeMap<RepoPath, StatusEntry> = BTreeMap::new();
    for (path, change) in staged {
        entries
            .entry(path.clone())
            .or_insert_with(|| empty(path))
            .staged = Some(change);
    }
    for (path, change) in unstaged {
        entries
            .entry(path.clone())
            .or_insert_with(|| empty(path))
            .unstaged = Some(change);
    }
    Ok(StatusSnapshot::from_entries(entries.into_values()))
}

fn empty(path: RepoPath) -> StatusEntry {
    StatusEntry {
        path,
        staged: None,
        unstaged: None,
    }
}

fn path_of(bytes: &gix::bstr::BStr) -> RepoPath {
    RepoPath(bytes.to_str_lossy().into_owned())
}

/// Index against `HEAD`.
fn staged_change(change: &gix::diff::index::Change) -> Option<(RepoPath, FileChange)> {
    use gix::diff::index::Change;
    Some(match change {
        Change::Addition { location, .. } => (path_of(location.as_ref()), FileChange::Added),
        Change::Deletion { location, .. } => (path_of(location.as_ref()), FileChange::Deleted),
        Change::Modification {
            location,
            previous_entry_mode,
            entry_mode,
            ..
        } => {
            let kind = |mode: &gix::index::entry::Mode| mode.to_tree_entry_mode().map(|m| m.kind());
            let change = if kind(previous_entry_mode) == kind(entry_mode)
                || is_file(previous_entry_mode) && is_file(entry_mode)
            {
                FileChange::Modified
            } else {
                FileChange::TypeChanged
            };
            (path_of(location.as_ref()), change)
        }
        // Rename tracking is off; a rewrite cannot appear.
        Change::Rewrite { .. } => return None,
    })
}

fn is_file(mode: &gix::index::entry::Mode) -> bool {
    use gix::index::entry::Mode;
    *mode == Mode::FILE || *mode == Mode::FILE_EXECUTABLE
}

/// Worktree against the index, plus untracked files.
fn worktree_change(item: &WorktreeItem) -> Option<(RepoPath, FileChange)> {
    match item {
        WorktreeItem::Modification {
            rela_path, status, ..
        } => {
            let change = match status {
                EntryStatus::Change(WorktreeChange::Removed) => FileChange::Deleted,
                EntryStatus::Change(WorktreeChange::Type { .. }) => FileChange::TypeChanged,
                EntryStatus::Change(_) => FileChange::Modified,
                // `git add -N`: the path is new in the worktree's eyes.
                EntryStatus::IntentToAdd => FileChange::Added,
                // A conflict is the phase's business; stat-only refreshes
                // are not changes.
                EntryStatus::Conflict { .. } | EntryStatus::NeedsUpdate(_) => return None,
            };
            Some((path_of(rela_path.as_ref()), change))
        }
        WorktreeItem::DirectoryContents { entry, .. } => {
            use gix::dir::entry::{Kind, Status};
            // Nested repositories and directories cannot be staged as text.
            let untracked = entry.status == Status::Untracked
                && matches!(entry.disk_kind, Some(Kind::File | Kind::Symlink));
            untracked.then(|| (path_of(entry.rela_path.as_ref()), FileChange::Untracked))
        }
        WorktreeItem::Rewrite { .. } => None,
    }
}
