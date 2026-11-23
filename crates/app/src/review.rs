//! The pre-push review (task 21) as words and small state: the title, what
//! the push is, the viewed checkmarks, and what makes the list stale. Pure;
//! `review_view` draws it and `shell::outgoing` reads it.

use std::collections::HashSet;

use invoke_core::{Head, Oid, RepoState, Snapshot};

/// What the diff beside the list shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Everything that would be pushed, as one diff.
    Combined,
    /// One outgoing commit on its own.
    PerCommit,
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// `1001+` when the list stopped at its limit.
fn count_text(count: usize, truncated: bool) -> String {
    if truncated {
        format!("{count}+")
    } else {
        count.to_string()
    }
}

pub fn title(count: usize, truncated: bool) -> String {
    match count {
        0 => "Nothing to push".to_owned(),
        1 if !truncated => "Review 1 commit before pushing".to_owned(),
        _ => format!(
            "Review {} commits before pushing",
            count_text(count, truncated)
        ),
    }
}

/// `main → origin/main · fast-forward`, or why it will not be.
pub fn subtitle(local: &str, target: &str, behind: u32, publishes: bool) -> String {
    let how = if publishes {
        "new branch on the remote".to_owned()
    } else if behind == 0 {
        "fast-forward".to_owned()
    } else {
        format!(
            "the remote has {}; fetch and rebase first",
            plural(behind as usize, "new commit", "new commits")
        )
    };
    format!("{local} → {target} · {how}")
}

/// The label of the button that sends it.
pub fn push_label(count: usize, truncated: bool, publishes: bool) -> String {
    let verb = if publishes { "Publish" } else { "Push" };
    match count {
        0 => verb.to_owned(),
        1 if !truncated => format!("{verb} 1 commit"),
        _ => format!("{verb} {} commits", count_text(count, truncated)),
    }
}

pub fn viewed_text(done: usize, total: usize) -> String {
    format!("{done} of {} viewed", plural(total, "file", "files"))
}

/// `+15` and `−3`; `None` (a file with no text diff) shows nothing.
pub fn counts(stats: Option<(u32, u32)>) -> Option<(String, String)> {
    stats.map(|(added, removed)| (format!("+{added}"), format!("−{removed}")))
}

/// Which files the reviewer ticked as viewed. A tick belongs to the diff it
/// was made in: the combined one, or one commit's.
#[derive(Debug, Default)]
pub struct Viewed(HashSet<(Option<String>, String)>);

impl Viewed {
    pub fn is(&self, commit: Option<&str>, path: &str) -> bool {
        self.0
            .contains(&(commit.map(str::to_owned), path.to_owned()))
    }

    /// Ticks or unticks; returns the new state.
    pub fn toggle(&mut self, commit: Option<&str>, path: &str) -> bool {
        let key = (commit.map(str::to_owned), path.to_owned());
        if self.0.remove(&key) {
            false
        } else {
            self.0.insert(key);
            true
        }
    }

    /// How many of `paths` are ticked in that diff.
    pub fn count<'a>(&self, commit: Option<&str>, paths: impl Iterator<Item = &'a str>) -> usize {
        paths.filter(|path| self.is(commit, path)).count()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// What the outgoing list was read from. When either moves (a fetch, a
/// commit, a push) the list is read again; never patched (rule 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub head: Oid,
    /// The upstream branch and where it points.
    pub upstream: Option<(String, Option<Oid>)>,
}

pub fn key_of(state: &RepoState) -> Option<Key> {
    let Snapshot::Read { disk, .. } = &state.snapshot else {
        return None;
    };
    let Head::Branch { name, oid } = &disk.head else {
        return None;
    };
    let upstream = disk.refs.upstreams.get(name).map(|upstream| {
        (
            upstream.name.0.clone(),
            disk.refs.refs.get(&upstream.name).cloned(),
        )
    });
    Some(Key {
        head: oid.clone(),
        upstream,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use invoke_core::{
        BranchName, DiskState, IndexId, Phase, ReadId, RefName, RefsSnapshot, RepoId,
        StatusSnapshot, Tracking, Upstream,
    };
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn the_title_counts_the_commits() {
        assert_eq!(title(0, false), "Nothing to push");
        assert_eq!(title(1, false), "Review 1 commit before pushing");
        assert_eq!(title(2, false), "Review 2 commits before pushing");
        assert_eq!(title(1000, true), "Review 1000+ commits before pushing");
    }

    #[test]
    fn the_subtitle_says_whether_it_will_go_through() {
        assert_eq!(
            subtitle("main", "origin/main", 0, false),
            "main → origin/main · fast-forward"
        );
        assert_eq!(
            subtitle("main", "origin/main", 1, false),
            "main → origin/main · the remote has 1 new commit; fetch and rebase first"
        );
        assert_eq!(
            subtitle("topic", "origin/topic", 0, true),
            "topic → origin/topic · new branch on the remote"
        );
    }

    #[test]
    fn the_button_says_what_it_sends() {
        assert_eq!(push_label(2, false, false), "Push 2 commits");
        assert_eq!(push_label(1, false, true), "Publish 1 commit");
        assert_eq!(push_label(0, false, false), "Push");
        assert_eq!(push_label(1000, true, false), "Push 1000+ commits");
    }

    #[test]
    fn counts_have_a_sign_and_none_means_nothing_to_show() {
        assert_eq!(counts(Some((15, 3))), Some(("+15".into(), "−3".into())));
        assert_eq!(counts(None), None);
        assert_eq!(viewed_text(1, 2), "1 of 2 files viewed");
        assert_eq!(viewed_text(0, 1), "0 of 1 file viewed");
    }

    #[test]
    fn a_tick_belongs_to_one_diff() {
        let mut viewed = Viewed::default();
        assert!(viewed.toggle(None, "a.rs"));
        assert!(viewed.is(None, "a.rs"));
        assert!(!viewed.is(Some("abc"), "a.rs"));
        assert!(!viewed.toggle(None, "a.rs"));
        assert!(!viewed.is(None, "a.rs"));
    }

    proptest! {
        /// The count of ticked files is the number of distinct ticked paths
        /// that are in the list, whatever the order of ticks and unticks.
        #[test]
        fn the_count_follows_the_ticks(
            clicks in proptest::collection::vec(0usize..5, 0..30),
        ) {
            let paths = ["a", "b", "c", "d", "e"];
            let mut viewed = Viewed::default();
            let mut state = [false; 5];
            for click in clicks {
                state[click] = viewed.toggle(None, paths[click]);
            }
            prop_assert_eq!(
                viewed.count(None, paths.iter().copied()),
                state.iter().filter(|on| **on).count()
            );
            prop_assert_eq!(viewed.count(Some("other"), paths.iter().copied()), 0);
        }
    }

    fn oid(c: char) -> Oid {
        Oid::from_hex(&c.to_string().repeat(40)).unwrap()
    }

    fn state(head: Head, refs: RefsSnapshot) -> RepoState {
        let mut state = RepoState::new(RepoId {
            root_commit: None,
            remote_url: None,
        });
        state.snapshot = Snapshot::Read {
            read: ReadId(1),
            disk: DiskState {
                head,
                phase: Phase::Clean,
                index: IndexId::Absent,
                status: StatusSnapshot::default(),
                refs,
                stashes: Vec::new(),
                lock: invoke_core::LockState::Free,
            },
        };
        state
    }

    fn refs(upstream_tip: char) -> RefsSnapshot {
        let name = RefName("refs/remotes/origin/main".into());
        RefsSnapshot {
            refs: BTreeMap::from([(name.clone(), oid(upstream_tip))]),
            upstreams: BTreeMap::from([(
                BranchName("main".into()),
                Upstream {
                    name,
                    tracking: Tracking::Apart {
                        ahead: 1,
                        behind: 0,
                    },
                },
            )]),
        }
    }

    #[test]
    fn the_list_is_read_again_when_the_head_or_the_upstream_moves() {
        let head = |c| Head::Branch {
            name: BranchName("main".into()),
            oid: oid(c),
        };
        let base = key_of(&state(head('a'), refs('b'))).unwrap();
        assert_eq!(base, key_of(&state(head('a'), refs('b'))).unwrap());
        assert_ne!(base, key_of(&state(head('c'), refs('b'))).unwrap());
        assert_ne!(base, key_of(&state(head('a'), refs('d'))).unwrap());
        // A detached head has nothing to review.
        assert!(key_of(&state(Head::Detached(oid('a')), refs('b'))).is_none());
    }
}
