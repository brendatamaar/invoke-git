//! Pure types, parsers, and planners for worktrees, submodules, and Git LFS (P10).
//!
//! No IO lives here: no filesystem, network, clock, threads, or randomness.
//! The shell reads `.git` and worktree metadata into [`AdvancedSnapshot`] and
//! executes CLI commands planned here through the system `git` CLI.

use crate::ids::{BranchName, Oid, RepoPath};
use crate::state::Head;

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
/// validates the `version`, `oid sha256:<64 lowercase hex>`, and `size <u64>`
/// fields per the Git LFS pointer specification.
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
    for &line in &raw_lines[1..] {
        if line.is_empty() {
            return None;
        }
        let (key, value) = line.split_once(' ')?;
        if key.is_empty() || value.is_empty() || value.contains(' ') {
            return None;
        }
        match key {
            "version" => return None, // duplicate version line
            "oid" => {
                if oid_sha256.is_some() {
                    return None;
                }
                let hex = value.strip_prefix("sha256:")?;
                if hex.len() != 64
                    || !hex
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                {
                    return None;
                }
                oid_sha256 = Some(hex.to_owned());
            }
            "size" => {
                if size.is_some() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                size = Some(value.parse::<u64>().ok()?);
            }
            ext if ext.starts_with("ext-") => {
                // Valid Git LFS pointer extension key.
            }
            _ => return None,
        }
    }
    Some(LfsPointer {
        oid_sha256: oid_sha256?,
        size: size?,
    })
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
