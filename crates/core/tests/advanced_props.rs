//! P10: property tests for worktrees, submodules, and Git LFS pure core logic.
//!
//! Pure: no filesystem, network, clock or randomness.

use std::path::Path;

use invoke_core::watch::WatchRoots;
use invoke_core::{
    Action, AdvancedSnapshot, Area, BranchName, BranchStart, Command, Conflicts, DiskState, Event,
    GitArgs, Head, IndexId, LFS_SPEC_VERSION, LfsFile, LfsFileState, LfsPointer, LfsRefusal,
    LfsSnapshot, LockState, MAX_LFS_POINTER_BYTES, Notice, Oid, OpOutcome, OpPhase, Phase, RefName,
    RefsSnapshot, Refusal, RepoId, RepoPath, RepoState, StatusSnapshot, SubmoduleEntry,
    SubmoduleOp, SubmoduleRefusal, SubmoduleState, WorktreeCheckout, WorktreeEntry,
    WorktreeRefusal, WorktreeSpec, classify_submodule_state, format_byte_size,
    gitattributes_uses_lfs, lfs_include_path_is_literal, normalize_worktree_path, open,
    parse_lfs_pointer, step, validate_worktree_path, worktree_holding_branch,
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

    /// Malformed `ext-` keys/values or unsorted keys are rejected per Git LFS pointer grammar.
    #[test]
    fn malformed_lfs_extensions_and_unsorted_keys_are_rejected(
        oid in arb_sha256(),
        size in 0u64..1_000_000,
        bad_ext in prop_oneof![
            Just("ext- foo".to_owned()),
            Just("ext-foo sha256:0123".to_owned()),
            Just("ext-0- sha256:0123".to_owned()),
            Just("ext-00-foo sha256:0123".to_owned()),
            Just("ext-0-BAD sha256:0123".to_owned()),
            Just("ext-0-foo notahash".to_owned()),
            Just("ext-0-foo sha256:short".to_owned()),
            Just("ext-0-foo sha256:ZZZZ".to_owned()),
        ],
    ) {
        let with_bad_ext = format!(
            "version {LFS_SPEC_VERSION}\n{bad_ext}\noid sha256:{oid}\nsize {size}\n"
        );
        prop_assert_eq!(parse_lfs_pointer(with_bad_ext.as_bytes()), None);

        // Unsorted keys (`size` before `oid`) are rejected.
        let unsorted = format!("version {LFS_SPEC_VERSION}\nsize {size}\noid sha256:{oid}\n");
        prop_assert_eq!(parse_lfs_pointer(unsorted.as_bytes()), None);
    }

    /// Per-file LFS pull rejects paths containing comma or glob characters.
    #[test]
    fn lfs_pull_refuses_comma_or_wildcard_paths(
        prefix in "[a-z0-9_/-]{0,8}",
        meta in prop_oneof![Just(','), Just('*'), Just('?'), Just('['), Just(']'), Just('\\')],
        suffix in "[a-z0-9_.-]{0,8}",
    ) {
        let path = RepoPath(format!("{prefix}{meta}{suffix}"));
        prop_assert!(!lfs_include_path_is_literal(&path));
        let disk = sample_disk(Phase::Clean, LockState::Free);
        let (state, cmds) = step(opened(disk), Event::LfsPullRequested { path: Some(path.clone()) });
        prop_assert!(cmds.iter().all(|c| !c.is_write()));
        prop_assert_eq!(
            state.notice,
            Some(Notice::Refused {
                action: Action::LfsPull,
                reason: Refusal::Lfs(LfsRefusal::UnsafeIncludePath(path)),
            })
        );
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
    let with_ext =
        format!("version {LFS_SPEC_VERSION}\next-0-foo sha256:{oid}\noid sha256:{oid}\nsize 42\n");
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
            index: None,
            is_main: true,
            is_current: true,
            locked: None,
            prunable: false,
        },
        WorktreeEntry {
            path: "/repo-feat".into(),
            name: "repo-feat".into(),
            head: Some(Head::Detached(oid)),
            index: None,
            is_main: false,
            is_current: false,
            locked: None,
            prunable: false,
        },
    ];
    assert!(worktree_holding_branch(&wts, &BranchName("main".into())).is_some());
    assert!(worktree_holding_branch(&wts, &BranchName("dev".into())).is_none());
}

fn sample_disk(phase: Phase, lock: LockState) -> DiskState {
    let oid1 = Oid::from_hex(&"1".repeat(40)).unwrap();
    let oid2 = Oid::from_hex(&"2".repeat(40)).unwrap();
    let mut refs = std::collections::BTreeMap::new();
    refs.insert(RefName("refs/heads/main".into()), oid1.clone());
    refs.insert(RefName("refs/heads/feature".into()), oid2.clone());
    DiskState {
        head: Head::Branch {
            name: BranchName("main".into()),
            oid: oid1.clone(),
        },
        phase,
        index: IndexId::Checksum(oid1.clone()),
        status: StatusSnapshot::default(),
        refs: RefsSnapshot {
            refs,
            ..RefsSnapshot::default()
        },
        stashes: Vec::new(),
        lock,
        advanced: AdvancedSnapshot {
            worktrees: vec![
                WorktreeEntry {
                    path: "/repo".into(),
                    name: "repo".into(),
                    head: Some(Head::Branch {
                        name: BranchName("main".into()),
                        oid: oid1.clone(),
                    }),
                    index: Some(IndexId::Checksum(oid1.clone())),
                    is_main: true,
                    is_current: true,
                    locked: None,
                    prunable: false,
                },
                WorktreeEntry {
                    path: "/repo-locked".into(),
                    name: "repo-locked".into(),
                    head: Some(Head::Detached(oid2.clone())),
                    index: Some(IndexId::Checksum(oid2.clone())),
                    is_main: false,
                    is_current: false,
                    locked: Some("pinned".into()),
                    prunable: false,
                },
            ],
            submodules: vec![SubmoduleEntry {
                name: "libs/child".into(),
                path: RepoPath("libs/child".into()),
                url: Some("https://example.com/child.git".into()),
                index_oid: Some(oid2),
                head_oid: None,
                head: None,
                state: SubmoduleState::Uninitialized,
            }],
            lfs: LfsSnapshot {
                detected: true,
                files: vec![LfsFile {
                    path: RepoPath("asset.bin".into()),
                    pointer: Some(LfsPointer {
                        oid_sha256: "a".repeat(64),
                        size: 512,
                    }),
                    state: LfsFileState::Pointer,
                }],
            },
        },
    }
}

fn opened(disk: DiskState) -> RepoState {
    let (state, commands) = open(RepoId {
        root_commit: None,
        remote_url: None,
    });
    let Command::ReadState { read, .. } = commands[0] else {
        panic!("expected ReadState");
    };
    let (state, _) = step(state, Event::StateRead { read, disk });
    state
}

proptest! {
    /// Any path that is empty, whitespace-only, starts with `-`, or contains
    /// control characters is rejected by `validate_worktree_path`.
    #[test]
    fn worktree_path_validation_rejects_flags_and_control_chars(
        text in prop_oneof!["[ \t]{0,6}", "-[a-z0-9/]{0,12}", "[a-z]{1,4}[\r\n\0][a-z]{1,4}"]
    ) {
        prop_assert!(validate_worktree_path(&text).is_err());
    }

    /// When a repo is busy (merge/rebase in progress), every P10 write is
    /// refused with `Refusal::RepoBusy` and emits no write commands.
    #[test]
    fn busy_repo_refuses_all_p10_operations(
        op in prop_oneof![
            Just(SubmoduleOp::Init),
            Just(SubmoduleOp::UpdateInitRecursive),
            Just(SubmoduleOp::SyncRecursive),
        ]
    ) {
        let busy_disk = sample_disk(
            Phase::Merging {
                conflicts: Conflicts::default(),
            },
            LockState::Free,
        );
        for (event, action) in [
            (
                Event::WorktreeAddRequested {
                    path: "/repo-new".into(),
                    spec: WorktreeSpec::ExistingBranch(BranchName("feature".into())),
                },
                Action::WorktreeAdd,
            ),
            (
                Event::WorktreeRemoveRequested {
                    path: "/repo-locked".into(),
                    force: true,
                },
                Action::WorktreeRemove,
            ),
            (
                Event::SubmoduleRequested { op, path: None },
                Action::Submodule,
            ),
            (Event::LfsPullRequested { path: None }, Action::LfsPull),
        ] {
            let state = opened(busy_disk.clone());
            let (state, commands) = step(state, event);
            prop_assert!(commands.iter().all(|c| !c.is_write()));
            prop_assert_eq!(
                state.notice,
                Some(Notice::Refused {
                    action,
                    reason: Refusal::RepoBusy
                })
            );
        }
    }

    /// Law 12 & Law 13: P10 operations wait while `index.lock` is held by
    /// another process, dispatch once free, can be cancelled while running,
    /// and trigger a fresh `ReadState` after cancellation.
    #[test]
    fn p10_ops_respect_lock_guard_and_support_cancellation(choice in 0u8..4) {
        let event = match choice {
            0 => Event::WorktreeAddRequested {
                path: "/repo-new".into(),
                spec: WorktreeSpec::ExistingBranch(BranchName("feature".into())),
            },
            1 => Event::WorktreeRemoveRequested {
                path: "/repo-locked".into(),
                force: true,
            },
            2 => Event::SubmoduleRequested {
                op: SubmoduleOp::UpdateInitRecursive,
                path: Some(RepoPath("libs/child".into())),
            },
            _ => Event::LfsPullRequested { path: None },
        };

        // 1. Held lock queues the operation without issuing any write command.
        let state = opened(sample_disk(Phase::Clean, LockState::HeldByOther));
        let (state, commands) = step(state, event);
        prop_assert!(commands.is_empty());
        prop_assert_eq!(state.ops.queued.len(), 1);
        prop_assert!(state.ops.running.is_none());

        // 2. Refresh sees lock freed -> op dispatches as `Command::RunGit`.
        let (state, commands) = step(state, Event::RefreshRequested);
        let Command::ReadState { read, .. } = commands[0] else {
            panic!("expected ReadState");
        };
        let (state, commands) = step(
            state,
            Event::StateRead {
                read,
                disk: sample_disk(Phase::Clean, LockState::Free),
            },
        );
        let run_op = commands.iter().find_map(|c| match c {
            Command::RunGit { op, .. } => Some(*op),
            _ => None,
        });
        let op = run_op.expect("expected RunGit once lock freed");
        prop_assert!(state.ops.running.is_some());

        // 3. Cancel emits `Command::CancelOp(op)` and enters `OpPhase::Cancelling`.
        let (state, commands) = step(state, Event::OpCancelRequested(op));
        prop_assert_eq!(commands, vec![Command::CancelOp(op)]);
        prop_assert_eq!(
            state.ops.running.as_ref().map(|r| &r.phase),
            Some(&OpPhase::Cancelling)
        );

        // 4. Law 13: OpFinished(Cancelled) triggers a fresh full ReadState.
        let (state, commands) = step(state, Event::OpFinished(op, OpOutcome::Cancelled));
        prop_assert!(state.ops.running.is_none());
        let has_read = commands
            .iter()
            .any(|c| matches!(c, Command::ReadState { .. }));
        prop_assert!(has_read);
    }
}

#[test]
fn worktree_submodule_and_lfs_refusals_cover_edge_cases() {
    let disk = sample_disk(Phase::Clean, LockState::Free);

    // Branch already checked out in a worktree is refused.
    let (s, _) = step(
        opened(disk.clone()),
        Event::WorktreeAddRequested {
            path: "/repo-dup".into(),
            spec: WorktreeSpec::ExistingBranch(BranchName("main".into())),
        },
    );
    assert_eq!(
        s.notice,
        Some(Notice::Refused {
            action: Action::WorktreeAdd,
            reason: Refusal::Worktree(WorktreeRefusal::BranchCheckedOut(BranchName("main".into())))
        })
    );

    // Removing main worktree is refused.
    let (s, _) = step(
        opened(disk.clone()),
        Event::WorktreeRemoveRequested {
            path: "/repo".into(),
            force: true,
        },
    );
    assert_eq!(
        s.notice,
        Some(Notice::Refused {
            action: Action::WorktreeRemove,
            reason: Refusal::Worktree(WorktreeRefusal::CannotRemoveMain)
        })
    );

    // Removing locked worktree without force is refused.
    let (s, _) = step(
        opened(disk.clone()),
        Event::WorktreeRemoveRequested {
            path: "/repo-locked".into(),
            force: false,
        },
    );
    assert_eq!(
        s.notice,
        Some(Notice::Refused {
            action: Action::WorktreeRemove,
            reason: Refusal::Worktree(WorktreeRefusal::Locked)
        })
    );

    // Dirty submodule refuses UpdateInitRecursive but allows SyncRecursive.
    let mut dirty_sub = disk.clone();
    dirty_sub.advanced.submodules[0].state = SubmoduleState::Dirty;
    let (s, cmds) = step(
        opened(dirty_sub.clone()),
        Event::SubmoduleRequested {
            op: SubmoduleOp::UpdateInitRecursive,
            path: None,
        },
    );
    assert!(cmds.iter().all(|c| !c.is_write()));
    assert_eq!(
        s.notice,
        Some(Notice::Refused {
            action: Action::Submodule,
            reason: Refusal::Submodule(SubmoduleRefusal::Dirty(RepoPath("libs/child".into())))
        })
    );
    let (_, sync_cmds) = step(
        opened(dirty_sub),
        Event::SubmoduleRequested {
            op: SubmoduleOp::SyncRecursive,
            path: None,
        },
    );
    assert!(sync_cmds.iter().any(|c| c.is_write()));

    // Submodule on a local branch refuses UpdateInitRecursive (Law 9).
    let mut branch_sub = disk.clone();
    let sub_oid = Oid::from_hex(&"2".repeat(40)).unwrap();
    branch_sub.advanced.submodules[0].head_oid = Some(sub_oid.clone());
    branch_sub.advanced.submodules[0].head = Some(Head::Branch {
        name: BranchName("topic".into()),
        oid: sub_oid,
    });
    branch_sub.advanced.submodules[0].state = SubmoduleState::Clean;
    let (s, cmds) = step(
        opened(branch_sub),
        Event::SubmoduleRequested {
            op: SubmoduleOp::UpdateInitRecursive,
            path: Some(RepoPath("libs/child".into())),
        },
    );
    assert!(cmds.iter().all(|c| !c.is_write()));
    assert_eq!(
        s.notice,
        Some(Notice::Refused {
            action: Action::Submodule,
            reason: Refusal::Submodule(SubmoduleRefusal::OnBranch {
                path: RepoPath("libs/child".into()),
                branch: BranchName("topic".into()),
            })
        })
    );

    // Unknown submodule path is refused.
    let (s, _) = step(
        opened(disk.clone()),
        Event::SubmoduleRequested {
            op: SubmoduleOp::SyncRecursive,
            path: Some(RepoPath("libs/nope".into())),
        },
    );
    assert_eq!(
        s.notice,
        Some(Notice::Refused {
            action: Action::Submodule,
            reason: Refusal::Submodule(SubmoduleRefusal::NotFound(RepoPath("libs/nope".into())))
        })
    );

    // LFS pull when LFS not detected is refused.
    let mut no_lfs = disk;
    no_lfs.advanced.lfs = LfsSnapshot::default();
    let (s, _) = step(opened(no_lfs), Event::LfsPullRequested { path: None });
    assert_eq!(
        s.notice,
        Some(Notice::Refused {
            action: Action::LfsPull,
            reason: Refusal::Lfs(LfsRefusal::NotDetected)
        })
    );

    // Expected ref guard is attached to WorktreeAdd for existing/new branches.
    let (s, cmds) = step(
        opened(sample_disk(Phase::Clean, LockState::Free)),
        Event::WorktreeAddRequested {
            path: "/repo-new".into(),
            spec: WorktreeSpec::NewBranch {
                name: "brand-new".into(),
                at: BranchStart::Head,
            },
        },
    );
    assert!(s.ops.running.is_some());
    let Some(Command::RunGit {
        args:
            args @ GitArgs::WorktreeAdd {
                checkout: WorktreeCheckout::NewBranch { .. },
                ..
            },
        ..
    }) = cmds.first()
    else {
        panic!("expected WorktreeAdd RunGit: {cmds:?}");
    };
    assert_eq!(args.expected_refs().len(), 1);
    assert_eq!(args.expected_refs()[0].oid, None);

    // WorktreeRemove carries target HEAD and index expectations from the snapshot.
    let (_, rm_cmds) = step(
        opened(sample_disk(Phase::Clean, LockState::Free)),
        Event::WorktreeRemoveRequested {
            path: "/repo-locked".into(),
            force: true,
        },
    );
    let oid2 = Oid::from_hex(&"2".repeat(40)).unwrap();
    let Some(Command::RunGit {
        args:
            GitArgs::WorktreeRemove {
                expect_head,
                expect_index,
                ..
            },
        ..
    }) = rm_cmds.first()
    else {
        panic!("expected WorktreeRemove RunGit: {rm_cmds:?}");
    };
    assert_eq!(*expect_head, Some(Head::Detached(oid2.clone())));
    assert_eq!(*expect_index, Some(IndexId::Checksum(oid2)));
}
