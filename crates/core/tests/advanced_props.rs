//! P10: property tests for worktrees, submodules, and Git LFS pure core logic.
//!
//! Pure: no filesystem, network, clock or randomness.

use std::path::Path;

use invoke_core::watch::WatchRoots;
use invoke_core::{Area, RepoId, RepoState, open};
use proptest::prelude::*;

proptest! {
    /// A linked worktree's own `HEAD`, `index`, `index.lock` and in-progress
    /// markers always classify into their respective areas, while the main
    /// worktree's own `HEAD` and `index` never trigger events in the linked
    /// worktree (isolating tabs opened on both worktrees).
    #[test]
    fn linked_worktree_watcher_isolates_main_worktree_state(
        wt_name in "[a-z][a-z0-9_-]{0,12}",
        file in "[a-z][a-z0-9_.-]{1,12}",
        case_insensitive in any::<bool>(),
    ) {
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
        let wt_file = format!("{workdir}{sep}{file}");
        let dot_git_file = format!("{workdir}{sep}.git");

        prop_assert_eq!(roots.classify(Path::new(&own_head)), Some(Area::Head));
        prop_assert_eq!(roots.classify(Path::new(&own_index)), Some(Area::Index));
        prop_assert_eq!(roots.classify(Path::new(&own_lock)), Some(Area::Index));
        prop_assert_eq!(roots.classify(Path::new(&shared_ref)), Some(Area::Refs));
        prop_assert_eq!(roots.classify(Path::new(&main_head)), None);
        prop_assert_eq!(roots.classify(Path::new(&main_index)), None);
        prop_assert_eq!(roots.classify(Path::new(&dot_git_file)), None);
        prop_assert_eq!(roots.classify(Path::new(&wt_file)), Some(Area::Worktree));
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
