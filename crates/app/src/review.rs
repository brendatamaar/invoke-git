//! The pre-push review (task 21) as words and small state: the title, what
//! the push is, the viewed checkmarks, and what makes the list stale. Pure;
//! `review_view` draws it and `shell::outgoing` reads it.

use std::collections::HashSet;

use invoke_core::notes::{self, Anchor, Note, NoteProblem};
use invoke_core::{BranchName, Head, Oid, RefName, RemoteBranch, RepoState, Snapshot};

use crate::sync;

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
        0 if publishes => "Publish branch".to_owned(),
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

/// The warning above the Push button while to-dos are still open.
pub fn todo_warning(open: usize) -> Option<String> {
    match open {
        0 => None,
        1 => Some("1 to-do note is unresolved".to_owned()),
        n => Some(format!("{n} to-do notes are unresolved")),
    }
}

/// The line number the user typed: a whole number from 1.
pub fn parse_line(text: &str) -> Result<u32, String> {
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|line| *line >= 1)
        .ok_or_else(|| "Type the line number, from 1.".to_owned())
}

pub fn problem_text(problem: &NoteProblem) -> String {
    match problem {
        NoteProblem::Empty => "Write something in the note.".to_owned(),
        NoteProblem::TooLong { max } => format!("A note can have at most {max} characters."),
        NoteProblem::NoSuchLine { lines } => format!(
            "This file has {} on this side.",
            plural(*lines, "line", "lines")
        ),
    }
}

/// `path:line`, as the list of notes shows where a note is.
pub fn place(note: &Note) -> String {
    format!("{}:{}", note.anchor.path, note.anchor.line)
}

/// The anchor for a note on `line_input` of the new side `new_text` of
/// `path`, written while `commit` was on screen.
pub fn new_anchor(
    commit: &str,
    path: &str,
    new_text: &str,
    line_input: &str,
) -> Result<Anchor, String> {
    let line = parse_line(line_input)?;
    let text = notes::line_of(new_text, line).map_err(|p| problem_text(&p))?;
    Ok(Anchor {
        commit: commit.to_owned(),
        path: path.to_owned(),
        line,
        text,
    })
}

/// Whether a note was written for this review: on one of the outgoing
/// commits, or on the tip (the combined diff).
pub fn is_in_review(note: &Note, commits: &[String], head: &str) -> bool {
    note.anchor.commit == head || commits.contains(&note.anchor.commit)
}

/// Notes of this branch whose commit is not part of this review any more (it
/// was rewritten) but whose file is still changed by it: candidates for
/// being found again by their line text.
pub fn adoptable<'a>(
    notes: &'a [Note],
    branch: &str,
    commits: &[String],
    head: &str,
    files: &[String],
) -> Vec<&'a Note> {
    notes
        .iter()
        .filter(|note| {
            note.branch == branch
                && !is_in_review(note, commits, head)
                && files.contains(&note.anchor.path)
        })
        .collect()
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

/// The local branch and remote refs that define the reviewed range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub local: BranchName,
    pub head: Oid,
    pub target: RemoteBranch,
    pub target_tip: Option<Oid>,
    pub sets_upstream: bool,
    /// With no target tip, the outgoing reader uses all remote-tracking refs.
    fallback_refs: Vec<(RefName, Oid)>,
}

impl Key {
    pub fn matches_outgoing(&self, outgoing: &shell::outgoing::Outgoing) -> bool {
        if outgoing.head != self.head.as_hex()
            || outgoing.upstream_tip.as_deref() != self.target_tip.as_ref().map(Oid::as_hex)
        {
            return false;
        }
        if self.target_tip.is_some() {
            return true;
        }
        let mut exclusions: Vec<_> = self
            .fallback_refs
            .iter()
            .map(|(_, id)| id.as_hex())
            .collect();
        exclusions.sort_unstable();
        exclusions.dedup();
        exclusions == outgoing.excluded_tips
    }
}

pub fn key_of(state: &RepoState) -> Option<Key> {
    let Snapshot::Read { disk, .. } = &state.snapshot else {
        return None;
    };
    let Head::Branch { name, oid } = &disk.head else {
        return None;
    };
    let plan = sync::plan(state)?;
    let target = plan.push_target()?;
    let target_tip = disk.refs.refs.get(&target.tracking_ref()).cloned();
    let fallback_refs = if target_tip.is_none() {
        disk.refs
            .refs
            .iter()
            .filter(|(name, _)| name.0.starts_with("refs/remotes/"))
            .map(|(name, oid)| (name.clone(), oid.clone()))
            .collect()
    } else {
        Vec::new()
    };
    Some(Key {
        local: name.clone(),
        head: oid.clone(),
        target,
        target_tip,
        sets_upstream: plan.publishes(),
        fallback_refs,
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

    fn note(commit: &str, branch: &str, path: &str, todo: bool, resolved: bool) -> Note {
        Note {
            id: notes::NoteId(1),
            anchor: Anchor {
                commit: commit.into(),
                path: path.into(),
                line: 3,
                text: "x".into(),
            },
            branch: branch.into(),
            body: "b".into(),
            todo,
            resolved,
            created: 0,
            updated: 0,
        }
    }

    #[test]
    fn the_warning_counts_open_todos() {
        assert_eq!(todo_warning(0), None);
        assert_eq!(
            todo_warning(1).as_deref(),
            Some("1 to-do note is unresolved")
        );
        assert_eq!(
            todo_warning(3).as_deref(),
            Some("3 to-do notes are unresolved")
        );
    }

    #[test]
    fn a_line_number_is_a_whole_number_from_one() {
        assert_eq!(parse_line(" 12 "), Ok(12));
        for bad in ["", "0", "-1", "1.5", "abc", "99999999999"] {
            assert!(parse_line(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_anchor_keeps_the_text_of_its_line() {
        let text = "one\ntwo\nthree\n";
        let anchor = new_anchor("abc", "a.rs", text, "2").unwrap();
        assert_eq!((anchor.line, anchor.text.as_str()), (2, "two"));
        assert_eq!(anchor.path, "a.rs");
        assert_eq!(
            new_anchor("abc", "a.rs", text, "4"),
            Err("This file has 3 lines on this side.".to_owned())
        );
        assert_eq!(
            new_anchor("abc", "a.rs", "x\n", "2"),
            Err("This file has 1 line on this side.".to_owned())
        );
    }

    #[test]
    fn only_notes_of_this_branch_and_file_are_found_again() {
        let commits = vec!["c1".to_owned(), "c2".to_owned()];
        let list = vec![
            note("c1", "topic", "a.rs", true, false),   // in the review
            note("head", "topic", "a.rs", true, false), // in the review (tip)
            note("old", "topic", "a.rs", true, false),  // rewritten: adoptable
            note("old", "other", "a.rs", true, false),  // another branch
            note("old", "topic", "gone.rs", true, false), // file no longer in the push
        ];
        let files = vec!["a.rs".to_owned(), "b.rs".to_owned()];
        let picked = adoptable(&list, "topic", &commits, "head", &files);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].anchor.commit, "old");
        assert!(is_in_review(&list[0], &commits, "head"));
        assert!(!is_in_review(&list[2], &commits, "head"));
        assert_eq!(place(&list[0]), "a.rs:3");
    }

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
        assert_eq!(push_label(0, false, true), "Publish branch");
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
                advanced: Default::default(),
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
            ..RefsSnapshot::default()
        }
    }

    proptest! {
        #[test]
        fn review_rejects_a_result_from_a_moved_target(changes in any::<bool>()) {
            let key = key_of(&state(Head::Branch {
                name: BranchName("main".into()), oid: oid('a'),
            }, refs('b'))).unwrap();
            let actual_tip = oid(if changes { 'c' } else { 'b' }).as_hex().to_owned();
            let outgoing = shell::outgoing::Outgoing {
                head: key.head.as_hex().to_owned(),
                upstream_tip: Some(actual_tip.clone()),
                excluded_tips: vec![actual_tip],
                base: None, commits: Vec::new(), truncated: false, files: Vec::new(),
            };
            prop_assert_eq!(key.matches_outgoing(&outgoing), !changes);
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
        let publishing = |tip| {
            key_of(&state(
                Head::Branch {
                    name: BranchName("topic".into()),
                    oid: oid('a'),
                },
                refs(tip),
            ))
            .unwrap()
        };
        assert_ne!(base, publishing('b'));
        assert_ne!(publishing('b'), publishing('d'));
        // A detached head has nothing to review.
        assert!(key_of(&state(Head::Detached(oid('a')), refs('b'))).is_none());
    }
}
