//! Repo-state detection rules (`docs/spec/core-design.md` → "Repo state
//! detection"), as pure functions over a description of what the shell found
//! in the git dir. The shell does the reading; the decisions live here.

use crate::state::{Conflicts, Phase};

/// Which in-progress markers exist in the worktree's own git dir, with the
/// raw text of their counter files. A missing directory is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Markers {
    pub rebase_merge: Option<RebaseFiles>,
    pub rebase_apply: Option<RebaseFiles>,
    pub merge_head: bool,
    pub cherry_pick_head: bool,
    pub revert_head: bool,
}

/// The two counter files inside a rebase directory: `msgnum` and `end` for
/// `rebase-merge/`, `next` and `last` for `rebase-apply/`. Missing files are
/// `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebaseFiles {
    pub step: Option<String>,
    pub total: Option<String>,
    pub edit: bool,
    pub origin: Option<crate::RebaseOrigin>,
}

/// Parses a counter file: a decimal number, optionally surrounded by
/// whitespace (git writes a trailing newline). Anything else is `None`.
pub fn parse_counter(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Applies the ordered rules; the first match decides. `conflicts` is what
/// the index says (stages 1-3) and is carried by every phase that can have
/// them.
///
/// A rebase directory whose counters are missing or unreadable (git creates
/// the directory a moment before it writes them) reports step and total 0.
/// When both rebase directories exist, `rebase-merge` wins.
pub fn detect_phase(markers: &Markers, conflicts: Conflicts) -> Phase {
    if let Some(files) = markers
        .rebase_merge
        .as_ref()
        .or(markers.rebase_apply.as_ref())
    {
        return Phase::Rebasing {
            step: files.step.as_deref().and_then(parse_counter).unwrap_or(0),
            total: files.total.as_deref().and_then(parse_counter).unwrap_or(0),
            pause: if !conflicts.all_resolved() {
                crate::RebasePause::Conflict
            } else if files.edit {
                crate::RebasePause::Edit
            } else if !conflicts.files().is_empty() {
                crate::RebasePause::Conflict
            } else {
                crate::RebasePause::Other
            },
            origin: files.origin.clone(),
            conflicts,
        };
    }
    if markers.merge_head {
        return Phase::Merging { conflicts };
    }
    if markers.cherry_pick_head {
        return Phase::CherryPicking { conflicts };
    }
    if markers.revert_head {
        return Phase::Reverting { conflicts };
    }
    Phase::Clean
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RepoPath;
    use proptest::prelude::*;

    fn files() -> impl Strategy<Value = Option<RebaseFiles>> {
        let text = proptest::option::of("\\PC{0,6}");
        proptest::option::of((text.clone(), text).prop_map(|(step, total)| RebaseFiles {
            step,
            total,
            ..RebaseFiles::default()
        }))
    }

    fn markers() -> impl Strategy<Value = Markers> {
        (
            files(),
            files(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(
                |(rebase_merge, rebase_apply, merge_head, cherry_pick_head, revert_head)| Markers {
                    rebase_merge,
                    rebase_apply,
                    merge_head,
                    cherry_pick_head,
                    revert_head,
                },
            )
    }

    fn conflicts() -> impl Strategy<Value = Conflicts> {
        proptest::collection::vec("[a-z/]{1,8}", 0..4).prop_map(|paths| {
            Conflicts::from_parts(
                paths
                    .into_iter()
                    .map(|path| (RepoPath(path), crate::conflict::ConflictKind::BothModified)),
                [],
            )
        })
    }

    fn carried(phase: &Phase) -> Option<&Conflicts> {
        match phase {
            Phase::Clean => None,
            Phase::Merging { conflicts }
            | Phase::CherryPicking { conflicts }
            | Phase::Reverting { conflicts }
            | Phase::Rebasing { conflicts, .. } => Some(conflicts),
        }
    }

    proptest! {
        #[test]
        fn counters_round_trip_with_git_whitespace(
            number in any::<u32>(),
            tail in prop_oneof![Just(""), Just("\n"), Just("\r\n"), Just(" \n")],
        ) {
            prop_assert_eq!(parse_counter(&format!("{number}{tail}")), Some(number));
        }

        #[test]
        fn counters_only_accept_plain_decimals(text in "\\PC{0,12}") {
            let trimmed = text.trim();
            let expected = !trimmed.is_empty()
                && trimmed.bytes().all(|byte| byte.is_ascii_digit())
                && trimmed.parse::<u32>().is_ok();
            prop_assert_eq!(parse_counter(&text).is_some(), expected);
        }

        #[test]
        fn first_matching_rule_wins(markers in markers(), conflicts in conflicts()) {
            let phase = detect_phase(&markers, conflicts.clone());
            let expected = if markers.rebase_merge.is_some() || markers.rebase_apply.is_some() {
                "rebasing"
            } else if markers.merge_head {
                "merging"
            } else if markers.cherry_pick_head {
                "cherry-picking"
            } else if markers.revert_head {
                "reverting"
            } else {
                "clean"
            };
            let actual = match phase {
                Phase::Rebasing { .. } => "rebasing",
                Phase::Merging { .. } => "merging",
                Phase::CherryPicking { .. } => "cherry-picking",
                Phase::Reverting { .. } => "reverting",
                Phase::Clean => "clean",
            };
            prop_assert_eq!(actual, expected);
            // Conflicts are carried through untouched, and `Clean` has none.
            match carried(&phase) {
                Some(found) => prop_assert_eq!(found, &conflicts),
                None => prop_assert_eq!(actual, "clean"),
            }
        }

        #[test]
        fn rebase_step_and_total_come_from_the_matching_directory(
            step in any::<u32>(),
            total in any::<u32>(),
            use_apply in any::<bool>(),
        ) {
            let dir = Some(RebaseFiles {
                step: Some(format!("{step}\n")),
                total: Some(format!("{total}\n")),
                ..RebaseFiles::default()
            });
            let markers = if use_apply {
                Markers { rebase_apply: dir, ..Markers::default() }
            } else {
                Markers { rebase_merge: dir, ..Markers::default() }
            };
            prop_assert_eq!(
                detect_phase(&markers, Conflicts::default()),
                Phase::Rebasing { step, total, conflicts: Conflicts::default(), pause: Default::default(), origin: None }
            );
        }

        #[test]
        fn rebase_merge_wins_over_rebase_apply(step in any::<u32>(), other in any::<u32>()) {
            let markers = Markers {
                rebase_merge: Some(RebaseFiles { step: Some(step.to_string()), total: None, ..RebaseFiles::default() }),
                rebase_apply: Some(RebaseFiles { step: Some(other.to_string()), total: None, ..RebaseFiles::default() }),
                ..Markers::default()
            };
            let Phase::Rebasing { step: found, .. } = detect_phase(&markers, Conflicts::default())
            else {
                panic!("not rebasing");
            };
            prop_assert_eq!(found, step);
        }
    }

    #[test]
    fn missing_counters_read_as_zero() {
        let markers = Markers {
            rebase_merge: Some(RebaseFiles::default()),
            ..Markers::default()
        };
        assert_eq!(
            detect_phase(&markers, Conflicts::default()),
            Phase::Rebasing {
                step: 0,
                total: 0,
                conflicts: Conflicts::default(),
                pause: Default::default(),
                origin: None
            }
        );
    }
}
