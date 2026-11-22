//! A repo's state in words: the header of an open repo tab. The formatting
//! is pure (state in, text out); only [`RepoSummary`] draws.

use gpui::{App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div, px};
use invoke_core::{Freshness, Head, LockState, Phase, RepoState, Snapshot, Tracking};
use tokens::{SPACE_2, SPACE_3, TEXT_BODY, TEXT_SMALL};

use crate::components::{Banner, BannerKind, Pill};
use crate::convert::hsla;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// What the header says about a repo, as plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// `main`, `Detached at 4f2a9c1` or `main (no commits yet)`.
    pub head: String,
    /// `↑2 ↓0 vs origin/main`, or `Upstream gone`; none without an upstream.
    pub tracking: Option<String>,
    /// `Merging: 2 conflicts`, `Rebasing 3/7: 1 conflict`; none when clean.
    pub phase: Option<String>,
    /// Files with a change in the index, and with a change in the worktree.
    pub staged: usize,
    pub unstaged: usize,
    /// `Another git process is running`, when `index.lock` is held by one.
    pub lock: Option<String>,
    /// Nothing has been read yet.
    pub reading: bool,
    /// The newest read failed, so what is shown may be out of date.
    pub stale: bool,
}

const SHORT_OID: usize = 7;

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn conflicts_text(unresolved: usize) -> String {
    if unresolved == 0 {
        String::new()
    } else {
        format!(": {}", plural(unresolved, "conflict", "conflicts"))
    }
}

impl Summary {
    pub fn of(state: &RepoState) -> Self {
        let stale = matches!(state.reads.freshness, Freshness::Failed { .. });
        let Snapshot::Read { disk, .. } = &state.snapshot else {
            return Self {
                head: String::new(),
                tracking: None,
                phase: None,
                staged: 0,
                unstaged: 0,
                lock: None,
                reading: true,
                stale,
            };
        };
        let head = match &disk.head {
            Head::Branch { name, .. } => name.0.clone(),
            Head::Detached(oid) => {
                let hex = oid.as_hex();
                format!("Detached at {}", &hex[..SHORT_OID.min(hex.len())])
            }
            Head::Unborn(name) => format!("{} (no commits yet)", name.0),
        };
        let tracking = match &disk.head {
            Head::Branch { name, .. } | Head::Unborn(name) => {
                disk.refs.upstreams.get(name).map(|upstream| {
                    let remote = upstream
                        .name
                        .0
                        .strip_prefix("refs/remotes/")
                        .unwrap_or(&upstream.name.0);
                    match upstream.tracking {
                        Tracking::Gone => format!("Upstream {remote} is gone"),
                        Tracking::Apart { ahead, behind } => {
                            format!("↑{ahead} ↓{behind} vs {remote}")
                        }
                    }
                })
            }
            Head::Detached(_) => None,
        };
        let phase = match &disk.phase {
            Phase::Clean => None,
            Phase::Merging { conflicts } => Some(format!(
                "Merging{}",
                conflicts_text(conflicts.unresolved_count())
            )),
            Phase::Rebasing {
                step,
                total,
                conflicts,
            } => Some(format!(
                "Rebasing {step}/{total}{}",
                conflicts_text(conflicts.unresolved_count())
            )),
            Phase::CherryPicking { conflicts } => Some(format!(
                "Cherry-picking{}",
                conflicts_text(conflicts.unresolved_count())
            )),
            Phase::Reverting { conflicts } => Some(format!(
                "Reverting{}",
                conflicts_text(conflicts.unresolved_count())
            )),
        };
        let lock = match disk.lock {
            LockState::HeldByOther => Some("Another git process is running".to_owned()),
            LockState::Free | LockState::HeldByUs => None,
        };
        Self {
            head,
            tracking,
            phase,
            staged: disk.status.staged().count(),
            unstaged: disk.status.unstaged().count(),
            lock,
            reading: false,
            stale,
        }
    }

    /// `3 staged · 5 not staged`, or `No changes`.
    pub fn changes(&self) -> String {
        match (self.staged, self.unstaged) {
            (0, 0) => "No changes".to_owned(),
            (staged, unstaged) => format!("{staged} staged · {unstaged} not staged"),
        }
    }

    /// The tab badge: the number of files with any change; empty for none.
    pub fn badge(&self) -> String {
        match self.staged.max(self.unstaged) {
            0 => String::new(),
            _ => (self.staged + self.unstaged).to_string(),
        }
    }
}

#[derive(IntoElement)]
pub struct RepoSummary {
    summary: Summary,
    path: SharedString,
    warning: Option<SharedString>,
}

impl RepoSummary {
    pub fn new(summary: Summary, path: impl Into<SharedString>) -> Self {
        Self {
            summary,
            path: path.into(),
            warning: None,
        }
    }

    /// A warning shown above the changes (for example, that the file watcher
    /// could not start).
    pub fn warning(mut self, text: impl Into<SharedString>) -> Self {
        self.warning = Some(text.into());
        self
    }
}

impl RenderOnce for RepoSummary {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let s = self.summary;
        let mut header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(SPACE_3))
            .w_full()
            .px(px(SPACE_3))
            .py(px(SPACE_2))
            .bg(hsla(p.bg_panel))
            .border_b_1()
            .border_color(hsla(p.line));
        if s.reading {
            header = header.child(
                div()
                    .text_style(&TEXT_BODY, cx)
                    .text_color(hsla(p.ink_muted))
                    .child("Reading the repository…"),
            );
        } else {
            header = header.child(Pill::new(s.head.clone()));
            if let Some(tracking) = &s.tracking {
                header = header.child(
                    div()
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(tracking.clone()),
                );
            }
        }

        header = header.child(div().flex_1()).child(
            div()
                .min_w(px(0.))
                .truncate()
                .text_style(&tokens::TEXT_CODE_SMALL, cx)
                .text_color(hsla(p.ink_faint))
                .child(self.path),
        );

        let mut body = div().flex().flex_col().flex_none().gap(px(SPACE_2));
        let mut any = false;
        if let Some(phase) = &s.phase {
            body = body.child(Banner::new(BannerKind::Warning, phase.clone()));
            any = true;
        }
        if let Some(warning) = self.warning {
            body = body.child(Banner::new(BannerKind::Warning, warning));
            any = true;
        }
        if let Some(lock) = &s.lock {
            body = body.child(Banner::new(BannerKind::Warning, lock.clone()));
            any = true;
        }
        if s.stale {
            body = body.child(Banner::new(
                BannerKind::Error,
                "The last read failed. What you see may be out of date.",
            ));
            any = true;
        }
        let banners = any.then(|| body.p(px(SPACE_2)).border_b_1().border_color(hsla(p.line)));
        div()
            .flex()
            .flex_col()
            .flex_none()
            .child(header)
            .children(banners)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use invoke_core::{
        BranchName, Conflicts, DiskState, Freshness, IndexId, Oid, ReadId, RefName, RefsSnapshot,
        RepoId, StatusSnapshot, Upstream,
    };
    use proptest::prelude::*;

    use super::*;

    fn state_with(disk: DiskState) -> RepoState {
        let mut state = RepoState::new(RepoId {
            root_commit: None,
            remote_url: None,
        });
        state.snapshot = Snapshot::Read {
            disk,
            read: ReadId(1),
        };
        state
    }

    fn disk(head: Head, phase: Phase, refs: RefsSnapshot) -> DiskState {
        DiskState {
            head,
            phase,
            index: IndexId::Absent,
            status: StatusSnapshot::default(),
            refs,
            stashes: Vec::new(),
            lock: LockState::Free,
        }
    }

    fn oid() -> Oid {
        Oid::from_hex("4f2a9c1d07b3e5a8c6f2d19e7b04a3c85d6e1f20").unwrap()
    }

    fn main_branch() -> BranchName {
        BranchName("main".to_owned())
    }

    #[test]
    fn a_new_state_is_reading() {
        let state = RepoState::new(RepoId {
            root_commit: None,
            remote_url: None,
        });
        let s = Summary::of(&state);
        assert!(s.reading);
        assert_eq!(s.changes(), "No changes");
    }

    #[test]
    fn tracking_reads_like_the_mockup() {
        let mut upstreams = BTreeMap::new();
        upstreams.insert(
            main_branch(),
            Upstream {
                name: RefName("refs/remotes/origin/main".to_owned()),
                tracking: Tracking::Apart {
                    ahead: 2,
                    behind: 0,
                },
            },
        );
        let refs = RefsSnapshot {
            refs: BTreeMap::new(),
            upstreams,
        };
        let state = state_with(disk(
            Head::Branch {
                name: main_branch(),
                oid: oid(),
            },
            Phase::Clean,
            refs,
        ));
        let s = Summary::of(&state);
        assert_eq!(s.head, "main");
        assert_eq!(s.tracking.as_deref(), Some("↑2 ↓0 vs origin/main"));
        assert_eq!(s.phase, None);
    }

    #[test]
    fn detached_and_unborn_heads_are_named() {
        let detached = Summary::of(&state_with(disk(
            Head::Detached(oid()),
            Phase::Clean,
            RefsSnapshot::default(),
        )));
        assert_eq!(detached.head, "Detached at 4f2a9c1");
        assert_eq!(detached.tracking, None);
        let unborn = Summary::of(&state_with(disk(
            Head::Unborn(main_branch()),
            Phase::Clean,
            RefsSnapshot::default(),
        )));
        assert_eq!(unborn.head, "main (no commits yet)");
    }

    #[test]
    fn a_rebase_shows_its_step_and_the_conflicts_left() {
        let conflicts = Conflicts::from_parts([], []);
        let s = Summary::of(&state_with(disk(
            Head::Branch {
                name: main_branch(),
                oid: oid(),
            },
            Phase::Rebasing {
                step: 3,
                total: 7,
                conflicts,
            },
            RefsSnapshot::default(),
        )));
        assert_eq!(s.phase.as_deref(), Some("Rebasing 3/7"));
    }

    #[test]
    fn a_failed_read_marks_the_summary_stale() {
        let mut state = state_with(disk(
            Head::Unborn(main_branch()),
            Phase::Clean,
            RefsSnapshot::default(),
        ));
        state.reads.freshness = Freshness::Failed { read: ReadId(2) };
        assert!(Summary::of(&state).stale);
    }

    proptest! {
        /// The tracking text always carries both counts and the remote.
        #[test]
        fn tracking_text_has_both_counts(ahead in 0u32..100_000, behind in 0u32..100_000) {
            let mut upstreams = BTreeMap::new();
            upstreams.insert(main_branch(), Upstream {
                name: RefName("refs/remotes/origin/main".to_owned()),
                tracking: Tracking::Apart { ahead, behind },
            });
            let refs = RefsSnapshot { refs: BTreeMap::new(), upstreams };
            let state = state_with(disk(
                Head::Branch { name: main_branch(), oid: oid() },
                Phase::Clean,
                refs,
            ));
            let text = Summary::of(&state).tracking.unwrap();
            prop_assert_eq!(text, format!("↑{ahead} ↓{behind} vs origin/main"));
        }
    }
}
