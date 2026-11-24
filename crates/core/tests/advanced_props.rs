//! P10: property tests for worktrees, submodules, and Git LFS pure core logic.
//!
//! Pure: no filesystem, network, clock or randomness.

use std::path::Path;

use invoke_core::watch::WatchRoots;
use invoke_core::{
    Area, BranchName, Head, LFS_SPEC_VERSION, LfsPointer, MAX_LFS_POINTER_BYTES, Oid, RepoId,
    RepoState, SubmoduleState, WorktreeEntry, classify_submodule_state, format_byte_size,
    gitattributes_uses_lfs, normalize_worktree_path, open, parse_lfs_pointer,
    worktree_holding_branch,
};
use proptest::prelude::*;

fn arb_sha256() -> impl Strategy<Value = String> {
    "[0-9a-f]{64}"
}

fn arb_oid() -> impl Strategy<Value = Oid> {
    "[0-9a-f]{40}".prop_map(|hex| Oid::from_hex(&hex).unwrap())
}

proptest! {
    /// A linked worktree's own `HEAD`, `index`, `index.lock` and in-progress
    /// markers always classify into their respective areas, while the main
    /// worktree's own `HEAD` and `index` never trigger events in the linked
    /// worktree (isolating tabs opened on both worktrees).
    #[test]
    fn linked_worktree_watcher_isolates_main_worktree_state(
        wt_name in "[a-z][a-z0-9_-]{0,12}",
        other_wt in "[a-z][a-z0-9_-]{0,12}",
        file in "[a-z][a-z0-9_.-]{1,12}",
        case_insensitive in any::<bool>(),
    ) {
        prop_assume!(wt_name != other_wt);
        let (workdir, git_dir, common_dir, sep) = if case_insensitive {
            (
                format!(r"C:\Repos\{wt_name}"),
                format!(r"C:\Repos\main\.git\worktrees\{wt_name}"),
                r"C:\Repos\main\.git".to_owned(),
                "\\",
            )
        } else {
            (
                format!("/repos/{wt_name}"),
                format!("/repos/main/.git/worktrees/{wt_name}"),
                "/repos/main/.git".to_owned(),
                "/",
            )
        };
        let roots = WatchRoots::new(
            Path::new(&workdir),
            Path::new(&git_dir),
            Path::new(&common_dir),
            case_insensitive,
        );

        let own_head = format!("{git_dir}{sep}HEAD");
        let own_index = format!("{git_dir}{sep}index");
        let own_lock = format!("{git_dir}{sep}index.lock");
        let main_head = format!("{common_dir}{sep}HEAD");
        let main_index = format!("{common_dir}{sep}index");
        let shared_ref = format!("{common_dir}{sep}refs{sep}heads{sep}main");
        let other_gitdir = format!("{common_dir}{sep}worktrees{sep}{other_wt}{sep}gitdir");
        let other_locked = format!("{common_dir}{sep}worktrees{sep}{other_wt}{sep}locked");
        let other_index = format!("{common_dir}{sep}worktrees{sep}{other_wt}{sep}index");
        let wt_file = format!("{workdir}{sep}{file}");
        let dot_git_file = format!("{workdir}{sep}.git");

        prop_assert_eq!(roots.classify(Path::new(&own_head)), Some(Area::Head));
        prop_assert_eq!(roots.classify(Path::new(&own_index)), Some(Area::Index));
        prop_assert_eq!(roots.classify(Path::new(&own_lock)), Some(Area::Index));
        prop_assert_eq!(roots.classify(Path::new(&shared_ref)), Some(Area::Refs));
        prop_assert_eq!(roots.classify(Path::new(&other_gitdir)), Some(Area::Refs));
        prop_assert_eq!(roots.classify(Path::new(&other_locked)), Some(Area::Refs));
        // Another worktree's index must not spam refreshes in this tab.
        prop_assert_eq!(roots.classify(Path::new(&other_index)), None);
        prop_assert_eq!(roots.classify(Path::new(&main_head)), None);
        prop_assert_eq!(roots.classify(Path::new(&main_index)), None);
        prop_assert_eq!(roots.classify(Path::new(&dot_git_file)), None);
        prop_assert_eq!(roots.classify(Path::new(&wt_file)), Some(Area::Worktree));
    }

    /// Valid Git LFS v1 pointers round-trip cleanly with both LF and CRLF endings.
    #[test]
    fn valid_lfs_pointers_round_trip(
        oid_sha256 in arb_sha256(),
        size in any::<u64>(),
        crlf in any::<bool>(),
        trailing_newline in any::<bool>(),
    ) {
        let pointer = LfsPointer { oid_sha256, size };
        let mut text = pointer.to_pointer_text();
        if !trailing_newline {
            text.pop();
        }
        if crlf {
            text = text.replace('\n', "\r\n");
        }
        prop_assert_eq!(parse_lfs_pointer(text.as_bytes()), Some(pointer));
    }

    /// Arbitrary bytes never panic `parse_lfs_pointer`, and payloads over 1024
    /// bytes or containing NUL bytes are always rejected.
    #[test]
    fn parse_lfs_pointer_is_total_and_enforces_limits(bytes in prop::collection::vec(any::<u8>(), 0..1200)) {
        let parsed = parse_lfs_pointer(&bytes);
        if bytes.len() > MAX_LFS_POINTER_BYTES || bytes.contains(&0) {
            prop_assert!(parsed.is_none());
        }
        if let Some(ptr) = parsed {
            prop_assert_eq!(ptr.oid_sha256.len(), 64);
            prop_assert!(ptr.oid_sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        }
    }

    /// Uppercase hex or wrong-length SHA-256 digests are rejected per Git LFS spec.
    #[test]
    fn invalid_lfs_oid_is_rejected(
        hex in "[0-9A-F]{64}|[0-9a-f]{1,63}|[0-9a-f]{65,70}",
        size in 0u64..1_000_000,
    ) {
        prop_assume!(hex.len() != 64 || hex.bytes().any(|b| b.is_ascii_uppercase()));
        let text = format!("version {LFS_SPEC_VERSION}\noid sha256:{hex}\nsize {size}\n");
        prop_assert_eq!(parse_lfs_pointer(text.as_bytes()), None);
    }

    /// `.gitattributes` detection honours comments and exact attribute tokens.
    #[test]
    fn gitattributes_lfs_detection_matches_active_rules(
        ext in "[a-z0-9]{1,8}",
        commented in any::<bool>(),
        suffix in prop_oneof![Just(""), Just("2"), Just("-extra")],
    ) {
        let prefix = if commented { "# " } else { "" };
        let text = format!("{prefix}*.{ext} filter=lfs{suffix} diff=lfs -text\n");
        let expected = !commented && suffix.is_empty();
        prop_assert_eq!(gitattributes_uses_lfs(&text), expected);
    }

    /// Submodule state classification follows the strict priority order:
    /// MergeConflict > Uninitialized > OutOfSync > Dirty > Clean.
    #[test]
    fn submodule_state_priority_holds(
        has_conflict in any::<bool>(),
        initialized in any::<bool>(),
        index_oid in prop::option::of(arb_oid()),
        head_oid in prop::option::of(arb_oid()),
        dirty in any::<bool>(),
    ) {
        let state = classify_submodule_state(
            has_conflict,
            initialized,
            index_oid.as_ref(),
            head_oid.as_ref(),
            dirty,
        );
        let expected = if has_conflict {
            SubmoduleState::MergeConflict
        } else if !initialized {
            SubmoduleState::Uninitialized
        } else if matches!((&index_oid, &head_oid), (Some(i), Some(h)) if i != h)
            || matches!((&index_oid, &head_oid), (Some(_), None))
        {
            SubmoduleState::OutOfSync
        } else if dirty {
            SubmoduleState::Dirty
        } else {
            SubmoduleState::Clean
        };
        prop_assert_eq!(state, expected);
    }

    /// Worktree path normalization is idempotent and ignores Windows `\\?\`
    /// prefixes, trailing slashes, and case when `case_insensitive` is true.
    #[test]
    fn worktree_path_normalization_is_idempotent(
        parts in prop::collection::vec("[A-Za-z0-9._-]{1,8}", 1..4),
        case_insensitive in any::<bool>(),
        trailing in 0usize..3,
    ) {
        let joined = format!(r"C:\{}", parts.join(r"\"));
        let decorated = format!(r"\\?\{joined}{}", r"\".repeat(trailing));
        let norm_plain = normalize_worktree_path(&joined, case_insensitive);
        let norm_dec = normalize_worktree_path(&decorated, case_insensitive);
        prop_assert_eq!(&norm_plain, &norm_dec);
        prop_assert_eq!(normalize_worktree_path(&norm_plain, case_insensitive), norm_plain);
    }
}

#[test]
fn opening_a_repo_starts_with_a_full_read_command() {
    let (state, commands) = open(RepoId {
        root_commit: None,
        remote_url: None,
    });
    assert_eq!(
        state.reads.next,
        invoke_core::ReadId(1),
        "first read id must be allocated"
    );
    assert_eq!(commands.len(), 1);
    assert!(matches!(
        state,
        RepoState {
            snapshot: invoke_core::Snapshot::NotRead,
            ..
        }
    ));
}

#[test]
fn lfs_pointer_supports_extensions_and_rejects_duplicates() {
    let oid = "a".repeat(64);
    let with_ext = format!(
        "version {LFS_SPEC_VERSION}\next-0-foo sha256:{oid}\noid sha256:{oid}\nsize 42\n"
    );
    assert_eq!(
        parse_lfs_pointer(with_ext.as_bytes()),
        Some(LfsPointer {
            oid_sha256: oid.clone(),
            size: 42
        })
    );
    let dup = format!("version {LFS_SPEC_VERSION}\noid sha256:{oid}\noid sha256:{oid}\nsize 1\n");
    assert_eq!(parse_lfs_pointer(dup.as_bytes()), None);
    assert_eq!(format_byte_size(512), "512 B");
    assert_eq!(format_byte_size(2048), "2.0 KiB");
}

#[test]
fn worktree_holding_branch_finds_checked_out_branch() {
    let oid = Oid::from_hex(&"1".repeat(40)).unwrap();
    let wts = vec![
        WorktreeEntry {
            path: "/repo".into(),
            name: "repo".into(),
            head: Some(Head::Branch {
                name: BranchName("main".into()),
                oid: oid.clone(),
            }),
            is_main: true,
            is_current: true,
            locked: None,
            prunable: false,
        },
        WorktreeEntry {
            path: "/repo-feat".into(),
            name: "repo-feat".into(),
            head: Some(Head::Detached(oid)),
            is_main: false,
            is_current: false,
            locked: None,
            prunable: false,
        },
    ];
    assert!(worktree_holding_branch(&wts, &BranchName("main".into())).is_some());
    assert!(worktree_holding_branch(&wts, &BranchName("dev".into())).is_none());
}
