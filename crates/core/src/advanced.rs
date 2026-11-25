//! Pure types, parsers, and planners for worktrees, submodules, and Git LFS (P10).
//!
//! No IO lives here: no filesystem, network, clock, threads, or randomness.
//! The shell reads `.git` and worktree metadata into [`AdvancedSnapshot`] and
//! executes CLI commands planned here through the system `git` CLI.

use crate::ids::{BranchName, IndexId, NameProblem, Oid, RepoPath};
use crate::ops::{BranchStart, GitArgs, WriteRequest};
use crate::state::{Action, DiskState, Head, Phase, Refusal};

/// Maximum byte length of a valid Git LFS v1 pointer file per the Git LFS spec.
pub const MAX_LFS_POINTER_BYTES: usize = 1024;

/// Version header value for Git LFS v1 pointers.
pub const LFS_SPEC_VERSION: &str = "https://git-lfs.github.com/spec/v1";

/// Worktree, submodule, and Git LFS state read from disk alongside [`crate::DiskState`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdvancedSnapshot {
    /// Primary worktree first, followed by linked worktrees sorted by path.
    pub worktrees: Vec<WorktreeEntry>,
    /// Submodules configured in `.gitmodules`, sorted by path.
    pub submodules: Vec<SubmoduleEntry>,
    /// Git LFS detection and tracked/pointer file status.
    pub lfs: LfsSnapshot,
}

/// One worktree belonging to this repository (main or linked).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    /// Absolute filesystem path to the worktree root.
    pub path: String,
    /// Short display name (directory name or admin id).
    pub name: String,
    /// Checked-out `HEAD` inside this worktree, if readable.
    pub head: Option<Head>,
    /// Index checksum inside this worktree, if readable (used as expected-state guard).
    pub index: Option<IndexId>,
    /// True for the primary worktree that owns `.git/`.
    pub is_main: bool,
    /// True for the worktree currently open in this [`crate::RepoState`].
    pub is_current: bool,
    /// Lock reason when the worktree is locked (`Some("")` if locked with no reason).
    pub locked: Option<String>,
    /// True when the worktree checkout directory is missing on disk.
    pub prunable: bool,
}

/// Operational status of a configured submodule (no booleans; enum covers all states).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmoduleState {
    /// Initialized, checked out at the superproject index commit, and clean.
    Clean,
    /// Recorded in `.gitmodules`/index, but not yet initialized (`-`).
    Uninitialized,
    /// Checked out at a different commit than recorded in the superproject index (`+`).
    OutOfSync,
    /// Checked out at the index commit, with modified or untracked files inside.
    Dirty,
    /// Superproject index has an unmerged conflict for this gitlink (`U`).
    MergeConflict,
}

/// One submodule configured in `.gitmodules`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmoduleEntry {
    /// Logical submodule name in `.gitmodules`.
    pub name: String,
    /// Path relative to the superproject worktree root (`/`-separated).
    pub path: RepoPath,
    /// Configured URL in `.gitmodules` or git config, if any.
    pub url: Option<String>,
    /// Commit OID recorded in the superproject index, if present.
    pub index_oid: Option<Oid>,
    /// Commit OID currently checked out at `HEAD` inside the submodule.
    pub head_oid: Option<Oid>,
    /// Checked-out `HEAD` inside the submodule (detached vs local branch), if initialized.
    pub head: Option<Head>,
    /// Derived status of this submodule.
    pub state: SubmoduleState,
}

/// A parsed Git LFS v1 pointer file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LfsPointer {
    /// 64 lowercase hexadecimal digits of the SHA-256 object digest.
    pub oid_sha256: String,
    /// Real binary object size in bytes.
    pub size: u64,
}

impl LfsPointer {
    /// Formats this pointer in canonical Git LFS v1 wire format.
    pub fn to_pointer_text(&self) -> String {
        format!(
            "version {LFS_SPEC_VERSION}\noid sha256:{}\nsize {}\n",
            self.oid_sha256, self.size
        )
    }

    /// Short 12-character prefix of the SHA-256 digest for UI badges.
    pub fn short_oid(&self) -> &str {
        &self.oid_sha256[..12.min(self.oid_sha256.len())]
    }
}

/// Whether a Git LFS file in the worktree is still a pointer, downloaded, or missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LfsFileState {
    /// Worktree holds the un-smudged pointer text (needs `git lfs pull`).
    Pointer,
    /// Worktree holds downloaded/smudged file contents.
    Downloaded,
    /// Tracked by `filter=lfs`, absent in the worktree.
    Missing,
}

/// Status of one Git LFS tracked or pointer file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LfsFile {
    pub path: RepoPath,
    pub pointer: Option<LfsPointer>,
    pub state: LfsFileState,
}

/// Repository-wide Git LFS snapshot read from disk (never runs `git-lfs`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LfsSnapshot {
    /// True when `.gitattributes` has `filter=lfs`, `.git/lfs` exists, or any
    /// LFS pointer file was detected.
    pub detected: bool,
    /// Detected LFS files, sorted by path.
    pub files: Vec<LfsFile>,
}

impl LfsSnapshot {
    /// Looks up LFS status for `path` (binary-searched by sorted path).
    pub fn file(&self, path: &RepoPath) -> Option<&LfsFile> {
        self.files
            .binary_search_by(|entry| entry.path.cmp(path))
            .ok()
            .map(|idx| &self.files[idx])
    }

    /// Count of files still in un-smudged pointer state.
    pub fn pointer_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.state == LfsFileState::Pointer)
            .count()
    }
}

/// Parses a Git LFS v1 pointer from raw bytes.
///
/// Accepts LF or CRLF line endings (Windows checkouts), rejects files larger
/// than [`MAX_LFS_POINTER_BYTES`], rejects non-UTF-8 or NUL bytes, and
/// validates the `version`, `oid sha256:<64 lowercase hex>`, `size <u64>`,
/// key ordering, and `ext-<priority>-<name> <method>:<hex>` grammar per the
/// Git LFS pointer specification.
pub fn parse_lfs_pointer(bytes: &[u8]) -> Option<LfsPointer> {
    if bytes.is_empty() || bytes.len() > MAX_LFS_POINTER_BYTES {
        return None;
    }
    if bytes.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut raw_lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if raw_lines.last() == Some(&"") {
        raw_lines.pop();
    }
    if raw_lines.is_empty() {
        return None;
    }
    if raw_lines[0] != format!("version {LFS_SPEC_VERSION}") {
        return None;
    }
    let mut oid_sha256: Option<String> = None;
    let mut size: Option<u64> = None;
    let mut prev_key: Option<&str> = None;
    for &line in &raw_lines[1..] {
        if line.is_empty() {
            return None;
        }
        let (key, value) = line.split_once(' ')?;
        if !is_valid_lfs_key(key)
            || value.is_empty()
            || value
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        {
            return None;
        }
        // Git LFS spec: all keys after `version` must be unique and sorted
        // in ascending lexicographical order.
        if let Some(prev) = prev_key
            && key <= prev
        {
            return None;
        }
        prev_key = Some(key);
        match key {
            "version" => return None,
            "oid" => {
                let hex = value.strip_prefix("sha256:")?;
                if !is_sha256_hex(hex) {
                    return None;
                }
                oid_sha256 = Some(hex.to_owned());
            }
            "size" => {
                if !is_canonical_u64(value) {
                    return None;
                }
                size = Some(value.parse::<u64>().ok()?);
            }
            ext if ext.starts_with("ext-") => {
                if !is_valid_lfs_ext_entry(ext, value) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    Some(LfsPointer {
        oid_sha256: oid_sha256?,
        size: size?,
    })
}

fn is_valid_lfs_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-'))
}

fn is_sha256_hex(hex: &str) -> bool {
    hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_canonical_u64(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text.len() == 1 || !text.starts_with('0'))
}

/// Validates a Git LFS extension line: `ext-<priority>-<name> <method>:<hex>`.
fn is_valid_lfs_ext_entry(key: &str, value: &str) -> bool {
    let Some(rest) = key.strip_prefix("ext-") else {
        return false;
    };
    let Some((priority, name)) = rest.split_once('-') else {
        return false;
    };
    if !is_canonical_u64(priority) || priority.parse::<u32>().is_err() || !is_valid_lfs_key(name) {
        return false;
    }
    let Some((method, hex)) = value.split_once(':') else {
        return false;
    };
    if method.is_empty()
        || !method
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9'))
    {
        return false;
    }
    if method == "sha256" {
        is_sha256_hex(hex)
    } else {
        !hex.is_empty() && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    }
}

/// Returns true when `path` can be passed literally to `git lfs pull --include=<path>`.
///
/// Git LFS splits `--include` on `,` and evaluates each entry as a wildcard
/// pattern (`*`, `?`, `[`, `]`, `\`). Paths containing any of those characters
/// cannot be targeted safely as a single-file `--include`.
pub fn lfs_include_path_is_literal(path: &RepoPath) -> bool {
    !path.0.is_empty()
        && !path
            .0
            .chars()
            .any(|c| matches!(c, ',' | '*' | '?' | '[' | ']' | '\\'))
}

/// Returns true if `.gitattributes` text configures `filter=lfs` on any active rule.
pub fn gitattributes_uses_lfs(text: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // First token is the pattern; subsequent whitespace-separated tokens are attributes.
        let mut tokens = trimmed.split_whitespace();
        let _pattern = tokens.next();
        if tokens.any(|attr| attr == "filter=lfs") {
            return true;
        }
    }
    false
}

/// Classifies a submodule's state from pure observations.
///
/// Priority order: `MergeConflict` > `Uninitialized` > `OutOfSync` > `Dirty` > `Clean`.
pub fn classify_submodule_state(
    has_conflict: bool,
    initialized: bool,
    index_oid: Option<&Oid>,
    head_oid: Option<&Oid>,
    dirty: bool,
) -> SubmoduleState {
    if has_conflict {
        return SubmoduleState::MergeConflict;
    }
    if !initialized {
        return SubmoduleState::Uninitialized;
    }
    match (index_oid, head_oid) {
        (Some(idx), Some(head)) if idx != head => SubmoduleState::OutOfSync,
        (Some(_), None) => SubmoduleState::OutOfSync,
        _ if dirty => SubmoduleState::Dirty,
        _ => SubmoduleState::Clean,
    }
}

/// Normalizes a worktree path into a comparable key (pure, no disk access).
///
/// Strips Windows `\\?\` / `\\?\UNC\` verbatim prefixes, converts `\` to `/`,
/// removes trailing slashes (except root `/` or `c:/`), and lowercases when
/// `case_insensitive` is true.
pub fn normalize_worktree_path(path: &str, case_insensitive: bool) -> String {
    let stripped = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        path.to_owned()
    };
    let mut normalized = stripped.replace('\\', "/");
    while normalized.len() > 1 && normalized.ends_with('/') && !normalized.ends_with(":/") {
        normalized.pop();
    }
    if case_insensitive {
        normalized = normalized.to_lowercase();
    }
    normalized
}

/// Whether two worktree paths refer to the same folder on this platform.
pub fn same_worktree_path(a: &str, b: &str) -> bool {
    let insensitive = cfg!(any(windows, target_os = "macos"));
    normalize_worktree_path(a, insensitive) == normalize_worktree_path(b, insensitive)
}

/// Returns the worktree that currently has `branch` checked out, if any.
pub fn worktree_holding_branch<'a>(
    worktrees: &'a [WorktreeEntry],
    branch: &BranchName,
) -> Option<&'a WorktreeEntry> {
    worktrees.iter().find(|wt| {
        matches!(
            &wt.head,
            Some(Head::Branch { name, .. } | Head::Unborn(name)) if name == branch
        )
    })
}

/// Formats a byte count for human display (e.g. `512 B`, `12.1 KiB`, `4.2 MiB`).
pub fn format_byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < MIB {
        format!("{:.1} KiB", b / KIB)
    } else if b < GIB {
        format!("{:.1} MiB", b / MIB)
    } else {
        format!("{:.2} GiB", b / GIB)
    }
}

/// What the user asked to check out in a new worktree (raw, before parsing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeSpec {
    /// Check out an existing local branch in the new worktree.
    ExistingBranch(BranchName),
    /// Create a new branch at `at` and check it out in the new worktree.
    NewBranch { name: String, at: BranchStart },
    /// Check out a detached `HEAD` at `at`.
    Detach(BranchStart),
}

/// Validated worktree checkout target carried inside [`WriteRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeTarget {
    ExistingBranch(BranchName),
    NewBranch { name: BranchName, at: BranchStart },
    Detach(BranchStart),
}

/// Resolved worktree target with concrete commit OIDs carried inside [`GitArgs`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeCheckout {
    ExistingBranch { name: BranchName, tip: Oid },
    NewBranch { name: BranchName, at: Oid },
    Detach(Oid),
}

/// Which submodule CLI action to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmoduleOp {
    /// `git submodule init [-- <path>]`
    Init,
    /// `git submodule update --init --recursive [-- <path>]`
    UpdateInitRecursive,
    /// `git submodule sync --recursive [-- <path>]`
    SyncRecursive,
}

/// Why a worktree request was refused by the pure core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeRefusal {
    EmptyPath,
    InvalidPath,
    InvalidBranch(NameProblem),
    NoCommitsYet,
    AlreadyExists,
    BranchAlreadyExists(BranchName),
    BranchNameConflict(BranchName),
    NoSuchBranch(BranchName),
    BranchCheckedOut(BranchName),
    NotFound,
    CannotRemoveMain,
    CannotRemoveCurrent,
    Locked,
}

/// Why a submodule request was refused by the pure core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmoduleRefusal {
    NoSubmodules,
    NotFound(RepoPath),
    /// Submodule has uncommitted or untracked changes that `update --checkout` could clobber.
    Dirty(RepoPath),
    /// Submodule has a local branch checked out; updating would detach or move it (Law 9).
    OnBranch {
        path: RepoPath,
        branch: BranchName,
    },
}

/// Why a Git LFS request was refused by the pure core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LfsRefusal {
    NotDetected,
    /// Path contains comma or wildcard characters that `--include=` cannot
    /// match literally.
    UnsafeIncludePath(RepoPath),
}

/// Validates a user-supplied filesystem path for a worktree operation.
pub fn validate_worktree_path(raw: &str) -> Result<String, WorktreeRefusal> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(WorktreeRefusal::EmptyPath);
    }
    if trimmed.starts_with('-') || trimmed.chars().any(char::is_control) {
        return Err(WorktreeRefusal::InvalidPath);
    }
    Ok(trimmed.to_owned())
}

/// Maps P10 write requests to their notice [`Action`]; `None` for other requests.
pub fn action_of(request: &WriteRequest) -> Option<Action> {
    match request {
        WriteRequest::WorktreeAdd { .. } => Some(Action::WorktreeAdd),
        WriteRequest::WorktreeRemove { .. } => Some(Action::WorktreeRemove),
        WriteRequest::Submodule { .. } => Some(Action::Submodule),
        WriteRequest::LfsPull { .. } => Some(Action::LfsPull),
        _ => None,
    }
}

/// Plans a P10 write request against `disk`. `None` for non-P10 requests.
pub fn plan(request: &WriteRequest, disk: &DiskState) -> Option<Result<GitArgs, Refusal>> {
    action_of(request)?;
    Some(check(request, disk))
}

fn check(request: &WriteRequest, disk: &DiskState) -> Result<GitArgs, Refusal> {
    if disk.phase != Phase::Clean {
        return Err(Refusal::RepoBusy);
    }
    match request {
        WriteRequest::WorktreeAdd { path, target } => plan_worktree_add(path, target, disk),
        WriteRequest::WorktreeRemove { path, force } => plan_worktree_remove(path, *force, disk),
        WriteRequest::Submodule { op, path } => plan_submodule(*op, path.as_ref(), disk),
        WriteRequest::LfsPull { path } => plan_lfs_pull(path.as_ref(), disk),
        _ => Err(Refusal::NotYetSupported { task: 0 }),
    }
}

fn resolve_start(start: &BranchStart, head: &Head) -> Result<Oid, WorktreeRefusal> {
    match start {
        BranchStart::Head => match head {
            Head::Branch { oid, .. } | Head::Detached(oid) => Ok(oid.clone()),
            Head::Unborn(_) => Err(WorktreeRefusal::NoCommitsYet),
        },
        BranchStart::Commit(oid) => Ok(oid.clone()),
    }
}

fn conflicting_branch(disk: &DiskState, name: &BranchName) -> Option<BranchName> {
    let folder_of = |outer: &str, inner: &str| {
        inner
            .strip_prefix(outer)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    disk.refs
        .refs
        .keys()
        .filter_map(|full| full.0.strip_prefix("refs/heads/"))
        .find(|existing| folder_of(existing, &name.0) || folder_of(&name.0, existing))
        .map(|existing| BranchName(existing.to_owned()))
}

fn plan_worktree_add(
    path: &str,
    target: &WorktreeTarget,
    disk: &DiskState,
) -> Result<GitArgs, Refusal> {
    let refuse = |why| Err(Refusal::Worktree(why));
    if disk
        .advanced
        .worktrees
        .iter()
        .any(|wt| same_worktree_path(&wt.path, path))
    {
        return refuse(WorktreeRefusal::AlreadyExists);
    }
    let checkout = match target {
        WorktreeTarget::ExistingBranch(name) => {
            let Some(tip) = disk.refs.refs.get(&name.to_ref()) else {
                return refuse(WorktreeRefusal::NoSuchBranch(name.clone()));
            };
            if worktree_holding_branch(&disk.advanced.worktrees, name).is_some() {
                return refuse(WorktreeRefusal::BranchCheckedOut(name.clone()));
            }
            WorktreeCheckout::ExistingBranch {
                name: name.clone(),
                tip: tip.clone(),
            }
        }
        WorktreeTarget::NewBranch { name, at } => {
            let at = match resolve_start(at, &disk.head) {
                Ok(oid) => oid,
                Err(why) => return refuse(why),
            };
            if disk.refs.refs.contains_key(&name.to_ref()) {
                return refuse(WorktreeRefusal::BranchAlreadyExists(name.clone()));
            }
            if let Some(other) = conflicting_branch(disk, name) {
                return refuse(WorktreeRefusal::BranchNameConflict(other));
            }
            WorktreeCheckout::NewBranch {
                name: name.clone(),
                at,
            }
        }
        WorktreeTarget::Detach(at) => {
            let at = match resolve_start(at, &disk.head) {
                Ok(oid) => oid,
                Err(why) => return refuse(why),
            };
            WorktreeCheckout::Detach(at)
        }
    };
    Ok(GitArgs::WorktreeAdd {
        path: path.to_owned(),
        checkout,
    })
}

fn plan_worktree_remove(path: &str, force: bool, disk: &DiskState) -> Result<GitArgs, Refusal> {
    let refuse = |why| Err(Refusal::Worktree(why));
    let Some(wt) = disk
        .advanced
        .worktrees
        .iter()
        .find(|wt| same_worktree_path(&wt.path, path))
    else {
        return refuse(WorktreeRefusal::NotFound);
    };
    if wt.is_main {
        return refuse(WorktreeRefusal::CannotRemoveMain);
    }
    if wt.is_current {
        return refuse(WorktreeRefusal::CannotRemoveCurrent);
    }
    if wt.locked.is_some() && !force {
        return refuse(WorktreeRefusal::Locked);
    }
    Ok(GitArgs::WorktreeRemove {
        path: wt.path.clone(),
        force,
        locked: wt.locked.is_some(),
        expect_head: wt.head.clone(),
        expect_index: wt.index.clone(),
    })
}

fn plan_submodule(
    op: SubmoduleOp,
    path: Option<&RepoPath>,
    disk: &DiskState,
) -> Result<GitArgs, Refusal> {
    if disk.advanced.submodules.is_empty() {
        return Err(Refusal::Submodule(SubmoduleRefusal::NoSubmodules));
    }
    if let Some(path) = path
        && !disk.advanced.submodules.iter().any(|s| &s.path == path)
    {
        return Err(Refusal::Submodule(SubmoduleRefusal::NotFound(path.clone())));
    }
    if op == SubmoduleOp::UpdateInitRecursive {
        for sub in disk
            .advanced
            .submodules
            .iter()
            .filter(|s| path.is_none_or(|target| &s.path == target))
        {
            if let Some(Head::Branch { name, .. } | Head::Unborn(name)) = &sub.head {
                return Err(Refusal::Submodule(SubmoduleRefusal::OnBranch {
                    path: sub.path.clone(),
                    branch: name.clone(),
                }));
            }
            if sub.state == SubmoduleState::Dirty {
                return Err(Refusal::Submodule(SubmoduleRefusal::Dirty(
                    sub.path.clone(),
                )));
            }
        }
    }
    Ok(GitArgs::Submodule {
        op,
        path: path.cloned(),
    })
}

fn plan_lfs_pull(path: Option<&RepoPath>, disk: &DiskState) -> Result<GitArgs, Refusal> {
    if !disk.advanced.lfs.detected {
        return Err(Refusal::Lfs(LfsRefusal::NotDetected));
    }
    if let Some(p) = path
        && !lfs_include_path_is_literal(p)
    {
        return Err(Refusal::Lfs(LfsRefusal::UnsafeIncludePath(p.clone())));
    }
    Ok(GitArgs::LfsPull {
        path: path.cloned(),
    })
}
