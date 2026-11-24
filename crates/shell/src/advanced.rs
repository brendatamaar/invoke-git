//! P10 reads: worktree list, submodule status, and Git LFS pointer/status
//! detection via `gix` and plain file reads (never spawns `git` or `git-lfs`
//! inside `read_state`, never takes `index.lock`).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use gix::bstr::ByteSlice;
use invoke_core::{
    AdvancedSnapshot, ErrorClass, Failure, Head, IndexId, LfsFile, LfsFileState, LfsPointer,
    LfsSnapshot, MAX_LFS_POINTER_BYTES, RepoPath, StatusSnapshot, SubmoduleEntry, WorktreeEntry,
    classify_submodule_state, gitattributes_uses_lfs, normalize_worktree_path, parse_lfs_pointer,
    same_worktree_path,
};

use crate::git::Git;
use crate::precondition::{self, RepoPaths};

/// Reads worktrees, submodules, and Git LFS metadata from disk.
///
/// Resilient against missing/prunable worktree directories and uninitialized
/// submodules so `read_state` never fails on partially initialized repos.
pub(crate) fn read_advanced(
    repo: &gix::Repository,
    worktree_repo: &gix::Repository,
    paths: &RepoPaths,
    status: &StatusSnapshot,
) -> AdvancedSnapshot {
    let worktrees = read_worktrees(repo, paths);
    let submodules = read_submodules(worktree_repo, paths);
    let lfs = read_lfs(repo, worktree_repo, paths, status);
    AdvancedSnapshot {
        worktrees,
        submodules,
        lfs,
    }
}

/// Checks whether `git-lfs` is installed by running `git lfs version`.
///
/// Not called during `read_state`; used on demand by callers/UI/tests so
/// routine state refreshes never spawn child processes.
pub fn git_lfs_version(git: &Git, workdir: &Path, env: &[(OsString, OsString)]) -> Option<String> {
    let mut cmd = git.command(workdir);
    cmd.args(["lfs", "version"])
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    text.starts_with("git-lfs/").then_some(text)
}

/// Inspects a single path for Git LFS pointer/tracked status (used by the diff
/// loader so binary LFS files still show size and OID in the diff viewer).
pub fn inspect_lfs_file(paths: &RepoPaths, path: &RepoPath) -> Option<LfsFile> {
    let repo = precondition::open_with_worktree(paths).ok()?;
    let index = repo.index_or_empty().ok()?;
    let index_ptr = index
        .entry_by_path(path.0.as_str().into())
        .and_then(|entry| read_pointer_blob(&repo, entry.id));

    let wt_path = paths.workdir.join(&path.0);
    let wt_info = inspect_worktree_lfs(&wt_path);
    let tracked = is_path_lfs_tracked(&repo, &index, path);

    match (tracked, index_ptr, wt_info) {
        (_, _, Some((LfsFileState::Pointer, Some(wt_ptr)))) => Some(LfsFile {
            path: path.clone(),
            pointer: Some(wt_ptr),
            state: LfsFileState::Pointer,
        }),
        (true, index_ptr, Some((state, wt_ptr))) => Some(LfsFile {
            path: path.clone(),
            pointer: wt_ptr.or(index_ptr),
            state,
        }),
        (true, index_ptr, None) => Some(LfsFile {
            path: path.clone(),
            pointer: index_ptr,
            state: LfsFileState::Missing,
        }),
        (false, Some(index_ptr), Some((state, wt_ptr))) => Some(LfsFile {
            path: path.clone(),
            pointer: wt_ptr.or(Some(index_ptr)),
            state,
        }),
        (false, Some(index_ptr), None) => Some(LfsFile {
            path: path.clone(),
            pointer: Some(index_ptr),
            state: LfsFileState::Missing,
        }),
        (false, None, _) => None,
    }
}

/// Returns true if `path` has `filter=lfs` configured in gitattributes.
pub(crate) fn is_path_lfs_tracked(
    repo: &gix::Repository,
    index: &gix::index::File,
    path: &RepoPath,
) -> bool {
    let Ok(mut stack) = repo.attributes_only(
        index,
        gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
    ) else {
        return false;
    };
    let mut outcome = gix::attrs::search::Outcome::default();
    outcome.initialize_with_selection(&Default::default(), ["filter"]);
    let Ok(platform) = stack.at_entry(path.0.as_str(), None) else {
        return false;
    };
    platform.matching_attributes(&mut outcome);
    outcome.iter_selected().any(|m| {
        m.assignment.name.as_str() == "filter"
            && matches!(
                m.assignment.state,
                gix::attrs::StateRef::Value(v) if v.as_bstr() == "lfs"
            )
    })
}

fn clean_absolute(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text = abs.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        abs
    }
}

fn folder_name(path: &Path, fallback: &str) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

fn read_worktrees(repo: &gix::Repository, paths: &RepoPaths) -> Vec<WorktreeEntry> {
    let common_dir = clean_absolute(repo.common_dir());
    let current_wd = clean_absolute(&paths.workdir);
    let current_str = current_wd.to_string_lossy().into_owned();
    let case_insensitive = cfg!(any(windows, target_os = "macos"));
    let hash_len = repo.object_hash().len_in_bytes();

    let mut result = Vec::new();

    // 1. Primary (main) worktree.
    if let Ok(main_repo) = repo.main_repo()
        && let Some(main_wd) = main_repo.workdir()
    {
        let main_path = clean_absolute(main_wd);
        let main_str = main_path.to_string_lossy().into_owned();
        let head = precondition::read_head(&main_repo).ok();
        let index = precondition::index_id(&main_repo.git_dir().join("index"), hash_len).ok();
        let is_current = same_worktree_path(&main_str, &current_str);
        result.push(WorktreeEntry {
            name: folder_name(&main_path, "main"),
            path: main_str,
            head,
            index,
            is_main: true,
            is_current,
            locked: None,
            prunable: !main_path.is_dir(),
        });
    } else if same_worktree_path(
        &clean_absolute(&paths.git_dir).to_string_lossy(),
        &common_dir.to_string_lossy(),
    ) {
        let head = precondition::read_head(repo).ok();
        let index = precondition::index_id(&paths.git_dir.join("index"), hash_len).ok();
        result.push(WorktreeEntry {
            name: folder_name(&current_wd, "main"),
            path: current_str.clone(),
            head,
            index,
            is_main: true,
            is_current: true,
            locked: None,
            prunable: !current_wd.is_dir(),
        });
    }

    // 2. Linked worktrees under `<common_dir>/worktrees/<id>`.
    let worktrees_dir = common_dir.join("worktrees");
    if let Ok(entries) = fs::read_dir(&worktrees_dir) {
        let mut linked = Vec::new();
        for entry in entries.flatten() {
            let wt_git_dir = entry.path();
            if !wt_git_dir.is_dir() {
                continue;
            }
            let admin_id = entry.file_name().to_string_lossy().into_owned();
            let Some((wt_root, gitfile_path)) = resolve_linked_worktree_root(&wt_git_dir) else {
                continue;
            };
            let wt_str = wt_root.to_string_lossy().into_owned();
            let locked = fs::read_to_string(wt_git_dir.join("locked"))
                .ok()
                .map(|reason| reason.trim().to_owned());
            let prunable = !wt_root.is_dir() || !gitfile_path.exists();
            let head = gix::open_opts(&wt_git_dir, precondition::options())
                .ok()
                .and_then(|wt_repo| precondition::read_head(&wt_repo).ok());
            let index = precondition::index_id(&wt_git_dir.join("index"), hash_len).ok();
            let is_current = same_worktree_path(&wt_str, &current_str);
            linked.push(WorktreeEntry {
                name: folder_name(&wt_root, &admin_id),
                path: wt_str,
                head,
                index,
                is_main: false,
                is_current,
                locked,
                prunable,
            });
        }
        linked.sort_by_key(|wt| normalize_worktree_path(&wt.path, case_insensitive));
        result.extend(linked);
    }

    result
}

fn resolve_linked_worktree_root(wt_git_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let gitdir_raw = fs::read_to_string(wt_git_dir.join("gitdir")).ok()?;
    let gitdir_trimmed = gitdir_raw.trim();
    if gitdir_trimmed.is_empty() {
        return None;
    }
    let gitfile_raw = PathBuf::from(gitdir_trimmed);
    let gitfile_path = if gitfile_raw.is_relative() {
        clean_absolute(&wt_git_dir.join(gitfile_raw))
    } else {
        clean_absolute(&gitfile_raw)
    };
    let wt_root = if gitfile_path
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case(".git"))
    {
        gitfile_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| gitfile_path.clone())
    } else {
        gitfile_path.clone()
    };
    Some((wt_root, gitfile_path))
}

/// Fresh on-disk state of a linked worktree about to be removed.
pub(crate) struct WorktreeRemoveTarget {
    pub workdir: PathBuf,
    pub dir_exists: bool,
    pub has_git_link: bool,
    pub locked_on_disk: bool,
}

/// P10: guards the target linked worktree itself before removal (Law 12):
/// verifies its `index.lock`, its expected `HEAD` and `index` from the snapshot,
/// its lock state, and checks on disk whether its directory actually exists.
pub(crate) fn check_worktree_remove_target(
    paths: &RepoPaths,
    target_path: &str,
    expect_head: Option<&Head>,
    expect_index: Option<&IndexId>,
    force: bool,
) -> Result<WorktreeRemoveTarget, Failure> {
    let refuse = |details: String| Failure {
        class: ErrorClass::PreconditionChanged,
        details,
    };
    let repo = precondition::open(paths).map_err(|details| Failure {
        class: ErrorClass::Bug,
        details,
    })?;
    let common_dir = clean_absolute(repo.common_dir());
    let hash_len = repo.object_hash().len_in_bytes();
    let worktrees_dir = common_dir.join("worktrees");

    let mut found = None;
    if let Ok(entries) = fs::read_dir(&worktrees_dir) {
        for entry in entries.flatten() {
            let wt_git_dir = entry.path();
            if !wt_git_dir.is_dir() {
                continue;
            }
            let Some((wt_root, gitfile_path)) = resolve_linked_worktree_root(&wt_git_dir) else {
                continue;
            };
            let wt_str = wt_root.to_string_lossy();
            if same_worktree_path(&wt_str, target_path) {
                found = Some((wt_git_dir, wt_root, gitfile_path));
                break;
            }
        }
    }

    let Some((wt_git_dir, wt_root, gitfile_path)) = found else {
        return Err(refuse(format!(
            "target worktree {target_path} is no longer registered"
        )));
    };

    let lock = wt_git_dir.join("index.lock");
    if lock.exists() {
        return Err(refuse(format!(
            "{} exists: another git process is running in worktree {}",
            lock.display(),
            target_path
        )));
    }

    let locked_on_disk = wt_git_dir.join("locked").exists();
    if locked_on_disk && !force {
        return Err(refuse(format!(
            "target worktree {target_path} is locked; tick Force remove to remove it"
        )));
    }

    let found_head = gix::open_opts(&wt_git_dir, precondition::options())
        .ok()
        .and_then(|wt_repo| precondition::read_head(&wt_repo).ok());
    if found_head.as_ref() != expect_head {
        return Err(refuse(format!(
            "target worktree HEAD moved: expected {expect_head:?}, found {found_head:?}"
        )));
    }

    let found_index = precondition::index_id(&wt_git_dir.join("index"), hash_len).ok();
    if found_index.as_ref() != expect_index {
        return Err(refuse(format!(
            "target worktree index changed: expected {expect_index:?}, found {found_index:?}"
        )));
    }

    Ok(WorktreeRemoveTarget {
        dir_exists: wt_root.exists(),
        has_git_link: gitfile_path.exists(),
        workdir: wt_root,
        locked_on_disk,
    })
}

/// Parses paths from `git status --porcelain=v1 --ignored=matching --untracked-files=all -z`.
pub(crate) fn parse_porcelain_z_paths(output: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut iter = output.split('\0');
    while let Some(entry) = iter.next() {
        if entry.len() < 4 {
            continue;
        }
        let bytes = entry.as_bytes();
        let x = bytes[0];
        let y = bytes[1];
        let path = &entry[3..];
        if !path.is_empty() {
            paths.push(path.to_owned());
        }
        // Renames/copies emit a second NUL-terminated path (the original path).
        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            let _ = iter.next();
        }
    }
    paths
}

fn read_submodules(worktree_repo: &gix::Repository, paths: &RepoPaths) -> Vec<SubmoduleEntry> {
    let Ok(Some(submodules)) = worktree_repo.submodules() else {
        return Vec::new();
    };
    let index = worktree_repo.index_or_empty().ok();
    let mut conflict_paths: BTreeSet<String> = BTreeSet::new();
    if let Some(index) = &index {
        for entry in index.entries() {
            if entry.stage_raw() != 0 {
                conflict_paths.insert(entry.path(index).to_str_lossy().into_owned());
            }
        }
    }

    let mut result = Vec::new();
    for sub in submodules {
        let name = sub.name().to_str_lossy().into_owned();
        let Ok(path_bstr) = sub.path() else {
            continue;
        };
        let path_str = path_bstr.to_str_lossy().into_owned();
        let path = RepoPath(path_str.clone());
        let url = sub.url().ok().map(|u| u.to_bstring().to_string());
        let has_conflict = conflict_paths.contains(&path_str);
        let index_oid = sub
            .index_id()
            .ok()
            .flatten()
            .and_then(|id| precondition::oid(&id).ok());

        let sub_workdir = paths.workdir.join(&path_str);
        let (initialized, head_oid, head, dirty) = if sub_workdir.join(".git").exists() {
            match gix::open_opts(&sub_workdir, precondition::options()) {
                Ok(sub_repo) => {
                    let head = precondition::read_head(&sub_repo).ok();
                    let head_oid = sub_repo
                        .head_id()
                        .ok()
                        .and_then(|id| precondition::oid(&id.detach()).ok());
                    let dirty = if !has_conflict && index_oid == head_oid {
                        crate::status::read_status(&sub_repo)
                            .map(|s| !s.entries.is_empty())
                            .unwrap_or(false)
                    } else {
                        false
                    };
                    (true, head_oid, head, dirty)
                }
                Err(_) => (false, None, None, false),
            }
        } else {
            (false, None, None, false)
        };

        let state = classify_submodule_state(
            has_conflict,
            initialized,
            index_oid.as_ref(),
            head_oid.as_ref(),
            dirty,
        );
        result.push(SubmoduleEntry {
            name,
            path,
            url,
            index_oid,
            head_oid,
            head,
            state,
        });
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    result
}

/// P10: fresh on-disk guard before `git submodule update --init --recursive --checkout`.
///
/// Refuses when any targeted initialized submodule (or nested submodule inside it)
/// holds `index.lock`, has a local branch checked out (Law 9), or has uncommitted
/// or untracked changes in its working tree.
pub(crate) fn check_submodule_update(
    paths: &RepoPaths,
    target: Option<&RepoPath>,
) -> Result<(), Failure> {
    let repo = precondition::open_with_worktree(paths)
        .or_else(|_| precondition::open_with_worktree_without_lfs_filter(paths))
        .map_err(|details| Failure {
            class: ErrorClass::Bug,
            details,
        })?;
    check_submodules_in(&repo, &paths.workdir, "", target)
}

fn check_submodules_in(
    repo: &gix::Repository,
    workdir: &Path,
    prefix: &str,
    target: Option<&RepoPath>,
) -> Result<(), Failure> {
    let Ok(Some(submodules)) = repo.submodules() else {
        return Ok(());
    };
    for sub in submodules {
        let Ok(path_bstr) = sub.path() else {
            continue;
        };
        let rel = path_bstr.to_str_lossy().into_owned();
        if let Some(filter) = target
            && rel != filter.0
        {
            continue;
        }
        let display_path = if prefix.is_empty() {
            rel.clone()
        } else {
            format!("{prefix}/{rel}")
        };
        let sub_workdir = workdir.join(&rel);
        if !sub_workdir.join(".git").exists() {
            continue;
        }
        let sub_repo = match gix::open_opts(&sub_workdir, precondition::options()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let lock = sub_repo.git_dir().join("index.lock");
        if lock.exists() {
            return Err(Failure {
                class: ErrorClass::PreconditionChanged,
                details: format!(
                    "{} exists: another git process is running in submodule {}",
                    lock.display(),
                    display_path
                ),
            });
        }
        if let Ok(Head::Branch { name, .. } | Head::Unborn(name)) =
            precondition::read_head(&sub_repo)
        {
            return Err(Failure {
                class: ErrorClass::Expected,
                details: format!(
                    "Refused to update submodule '{}': local branch '{}' is checked out. Detach HEAD before updating.",
                    display_path, name.0
                ),
            });
        }
        let status_res = crate::status::read_status(&sub_repo).or_else(|_| {
            let sub_paths = RepoPaths {
                workdir: sub_workdir.clone(),
                git_dir: sub_repo.git_dir().to_path_buf(),
            };
            precondition::open_with_worktree_without_lfs_filter(&sub_paths)
                .and_then(|r| crate::status::read_status(&r))
        });
        if let Ok(status) = status_res
            && !status.entries.is_empty()
        {
            return Err(Failure {
                class: ErrorClass::Expected,
                details: format!(
                    "Refused to update submodule '{}': working tree has uncommitted or untracked changes.",
                    display_path
                ),
            });
        }
        // Check nested submodules recursively (all of them once the parent matched).
        check_submodules_in(&sub_repo, &sub_workdir, &display_path, None)?;
    }
    Ok(())
}

fn read_lfs(
    repo: &gix::Repository,
    worktree_repo: &gix::Repository,
    paths: &RepoPaths,
    status: &StatusSnapshot,
) -> LfsSnapshot {
    let has_lfs_dir = repo.common_dir().join("lfs").is_dir();
    let index = worktree_repo.index_or_empty().ok();
    let has_lfs_attrs = repo_attributes_use_lfs(repo, paths, index.as_deref().map(|v| &**v));

    let mut files: BTreeMap<RepoPath, LfsFile> = BTreeMap::new();

    if has_lfs_attrs
        && let Some(index) = &index
        && let Ok(mut stack) = worktree_repo.attributes_only(
            index,
            gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
        )
    {
        let mut outcome = gix::attrs::search::Outcome::default();
        outcome.initialize_with_selection(&Default::default(), ["filter"]);
        for entry in index.entries() {
            use gix::index::entry::Mode;
            if entry.stage_raw() != 0 || !matches!(entry.mode, Mode::FILE | Mode::FILE_EXECUTABLE) {
                continue;
            }
            let rel = entry.path(index);
            let Ok(platform) = stack.at_entry(rel, None) else {
                continue;
            };
            platform.matching_attributes(&mut outcome);
            let is_lfs = outcome.iter_selected().any(|m| {
                m.assignment.name.as_str() == "filter"
                    && matches!(
                        m.assignment.state,
                        gix::attrs::StateRef::Value(v) if v.as_bstr() == "lfs"
                    )
            });
            if !is_lfs {
                continue;
            }
            let rel_str = rel.to_str_lossy().into_owned();
            let repo_path = RepoPath(rel_str.clone());
            let index_ptr = read_pointer_blob(worktree_repo, entry.id);
            let wt_path = paths.workdir.join(&rel_str);
            let (state, pointer) = match inspect_worktree_lfs(&wt_path) {
                Some((state, wt_ptr)) => (state, wt_ptr.or(index_ptr)),
                None => (LfsFileState::Missing, index_ptr),
            };
            files.insert(
                repo_path.clone(),
                LfsFile {
                    path: repo_path,
                    pointer,
                    state,
                },
            );
        }
    }

    // Also detect any changed/untracked LFS pointer file even if `.gitattributes`
    // is missing or not yet committed.
    for entry in &status.entries {
        if files.contains_key(&entry.path) {
            continue;
        }
        let wt_path = paths.workdir.join(&entry.path.0);
        if let Some((LfsFileState::Pointer, Some(ptr))) = inspect_worktree_lfs(&wt_path) {
            files.insert(
                entry.path.clone(),
                LfsFile {
                    path: entry.path.clone(),
                    pointer: Some(ptr),
                    state: LfsFileState::Pointer,
                },
            );
            continue;
        }
        if let Some(index) = &index
            && let Some(idx_entry) = index.entry_by_path(entry.path.0.as_str().into())
            && let Some(ptr) = read_pointer_blob(worktree_repo, idx_entry.id)
        {
            let state = match inspect_worktree_lfs(&wt_path) {
                Some((state, _)) => state,
                None => LfsFileState::Missing,
            };
            files.insert(
                entry.path.clone(),
                LfsFile {
                    path: entry.path.clone(),
                    pointer: Some(ptr),
                    state,
                },
            );
        }
    }

    let detected = has_lfs_dir || has_lfs_attrs || !files.is_empty();
    LfsSnapshot {
        detected,
        files: files.into_values().collect(),
    }
}

fn repo_attributes_use_lfs(
    repo: &gix::Repository,
    paths: &RepoPaths,
    index: Option<&gix::index::File>,
) -> bool {
    let candidates = [
        paths.workdir.join(".gitattributes"),
        paths.git_dir.join("info/attributes"),
        repo.common_dir().join("info/attributes"),
    ];
    for path in candidates {
        if let Ok(text) = fs::read_to_string(&path)
            && gitattributes_uses_lfs(&text)
        {
            return true;
        }
    }
    if let Some(index) = index {
        for entry in index.entries() {
            let rel = entry.path(index);
            if rel.ends_with(b"/.gitattributes") || rel == b".gitattributes" {
                let wt = paths.workdir.join(rel.to_str_lossy().as_ref());
                if let Ok(text) = fs::read_to_string(&wt)
                    && gitattributes_uses_lfs(&text)
                {
                    return true;
                }
                if let Ok(obj) = repo.find_object(entry.id)
                    && let Ok(text) = std::str::from_utf8(&obj.data)
                    && gitattributes_uses_lfs(text)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Reads an object blob and parses it as an LFS pointer only when its header
/// size is within [`MAX_LFS_POINTER_BYTES`], avoiding reading large binaries.
fn read_pointer_blob(repo: &gix::Repository, id: gix::ObjectId) -> Option<LfsPointer> {
    let header = repo.find_header(id).ok()?;
    if header.kind() != gix::object::Kind::Blob || header.size() > MAX_LFS_POINTER_BYTES as u64 {
        return None;
    }
    let obj = repo.find_object(id).ok()?;
    parse_lfs_pointer(&obj.data)
}

/// Inspects a worktree file without reading multi-megabyte binaries.
/// Returns `None` if the file does not exist on disk.
fn inspect_worktree_lfs(path: &Path) -> Option<(LfsFileState, Option<LfsPointer>)> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_LFS_POINTER_BYTES as u64 {
        return Some((LfsFileState::Downloaded, None));
    }
    let bytes = fs::read(path).ok()?;
    match parse_lfs_pointer(&bytes) {
        Some(ptr) => Some((LfsFileState::Pointer, Some(ptr))),
        None => Some((LfsFileState::Downloaded, None)),
    }
}
