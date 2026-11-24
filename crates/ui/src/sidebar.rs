//! The repository sidebar (backlog task 15, and the stash section of task
//! 20): local branches with their ahead and behind counts, remote branches
//! and tags, in sections. The rows are pure (refs in, rows out); the
//! component draws them and reports what the user asked for.

use std::rc::Rc;

use gpui::{
    App, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window, div, px, uniform_list,
};
use invoke_core::{AdvancedSnapshot, Head, RefsSnapshot, StashEntry, SubmoduleState, Tracking};
use tokens::{RADIUS_MD, RADIUS_SM, ROW_LIST, SPACE_1, SPACE_2, TEXT_BODY, TEXT_LABEL, TEXT_SMALL};

use crate::convert::hsla;
use crate::focus;
use crate::scrollbar::Scrollbar;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// One row of the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SideRow {
    Header(String),
    Local {
        name: String,
        current: bool,
        /// `↑2 ↓0`, or `gone`; none for a branch without an upstream.
        tracking: Option<String>,
    },
    /// `origin/main`.
    Remote(String),
    Tag(String),
    /// A stash entry: its position, a one-line label.
    Stash {
        index: usize,
        label: String,
    },
    /// P10: a git worktree (main or linked).
    Worktree {
        path: String,
        label: String,
        branch: String,
        main: bool,
        current: bool,
        locked: bool,
        prunable: bool,
    },
    /// P10: a configured submodule with its operational status.
    Submodule {
        path: String,
        name: String,
        status: String,
        state: SubmoduleState,
    },
}

/// What the user asked for in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SideAction {
    /// Make the local branch the checked-out one.
    Switch(String),
    Rename(String),
    Delete(String),
    /// Create a local branch from the remote branch (`origin/main`) and
    /// switch to it.
    CheckOutRemote(String),
    ManageTag(String),
    /// Apply, pop or drop the stash entry at this position.
    StashApply(usize),
    StashPop(usize),
    StashDrop(usize),
    /// P10: open a linked/main worktree in its own tab, or remove a linked one.
    WorktreeOpen(String),
    WorktreeRemove(String),
    /// P10: update (`--init --recursive`) or sync (`--recursive`) a submodule.
    SubmoduleUpdate(Option<String>),
    SubmoduleSync(Option<String>),
}

fn tracking_text(tracking: Tracking) -> String {
    match tracking {
        Tracking::Gone => "gone".to_owned(),
        Tracking::Apart { ahead, behind } => format!("↑{ahead} ↓{behind}"),
    }
}

fn worktree_branch_label(head: Option<&Head>, locked: bool, prunable: bool) -> String {
    let base = match head {
        Some(Head::Branch { name, .. } | Head::Unborn(name)) => name.0.clone(),
        Some(Head::Detached(oid)) => oid.as_hex()[..7.min(oid.as_hex().len())].to_owned(),
        None => "unknown".to_owned(),
    };
    if prunable {
        format!("{base} · missing")
    } else if locked {
        format!("{base} · locked")
    } else {
        base
    }
}

fn submodule_status_label(state: SubmoduleState) -> &'static str {
    match state {
        SubmoduleState::Clean => "clean",
        SubmoduleState::Uninitialized => "uninit",
        SubmoduleState::OutOfSync => "out of sync",
        SubmoduleState::Dirty => "dirty",
        SubmoduleState::MergeConflict => "conflict",
    }
}

/// The rows for the refs: LOCAL (the current branch first, then the others
/// by name), one REMOTE section per remote, TAGS, STASHES, WORKTREES, and
/// SUBMODULES when there are any. A section without entries has no rows.
pub fn sidebar_rows(
    refs: &RefsSnapshot,
    head: &Head,
    stashes: &[StashEntry],
    advanced: &AdvancedSnapshot,
) -> Vec<SideRow> {
    let current = match head {
        Head::Branch { name, .. } | Head::Unborn(name) => Some(name.0.as_str()),
        Head::Detached(_) => None,
    };
    let mut locals: Vec<(&str, Option<String>)> = Vec::new();
    let mut remotes: Vec<(&str, &str)> = Vec::new(); // (remote, branch)
    for name in refs.refs.keys() {
        let full = name.0.as_str();
        if let Some(branch) = full.strip_prefix("refs/heads/") {
            let tracking = refs
                .upstreams
                .get(&invoke_core::BranchName(branch.to_owned()))
                .map(|upstream| tracking_text(upstream.tracking));
            locals.push((branch, tracking));
        } else if let Some(rest) = full.strip_prefix("refs/remotes/")
            && let Some((remote, branch)) = rest.split_once('/')
            && branch != "HEAD"
        {
            remotes.push((remote, branch));
        }
    }
    // An unborn branch has no ref yet but is still the current one.
    if let Some(name) = current
        && !locals.iter().any(|(n, _)| *n == name)
    {
        locals.push((name, None));
    }
    locals.sort_by_key(|(name, _)| (Some(*name) != current, *name));

    let mut rows = Vec::new();
    if !locals.is_empty() {
        rows.push(SideRow::Header("LOCAL".to_owned()));
        for (name, tracking) in locals {
            rows.push(SideRow::Local {
                name: name.to_owned(),
                current: Some(name) == current,
                tracking,
            });
        }
    }
    remotes.sort();
    let mut last: Option<&str> = None;
    for (remote, branch) in remotes {
        if last != Some(remote) {
            rows.push(SideRow::Header(format!(
                "REMOTE · {}",
                remote.to_uppercase()
            )));
            last = Some(remote);
        }
        rows.push(SideRow::Remote(format!("{remote}/{branch}")));
    }
    let tags = refs.tags();
    if !tags.is_empty() {
        rows.push(SideRow::Header("TAGS".to_owned()));
        rows.extend(tags.into_iter().map(|tag| SideRow::Tag(tag.name.0)));
    }
    if !stashes.is_empty() {
        rows.push(SideRow::Header("STASHES".to_owned()));
        for (index, entry) in stashes.iter().enumerate() {
            rows.push(SideRow::Stash {
                index,
                label: entry.message.clone(),
            });
        }
    }
    if !advanced.worktrees.is_empty() {
        rows.push(SideRow::Header("WORKTREES".to_owned()));
        for wt in &advanced.worktrees {
            rows.push(SideRow::Worktree {
                path: wt.path.clone(),
                label: wt.name.clone(),
                branch: worktree_branch_label(wt.head.as_ref(), wt.locked.is_some(), wt.prunable),
                main: wt.is_main,
                current: wt.is_current,
                locked: wt.locked.is_some(),
                prunable: wt.prunable,
            });
        }
    }
    if !advanced.submodules.is_empty() {
        rows.push(SideRow::Header("SUBMODULES".to_owned()));
        for sub in &advanced.submodules {
            rows.push(SideRow::Submodule {
                path: sub.path.0.clone(),
                name: sub.name.clone(),
                status: submodule_status_label(sub.state).to_owned(),
                state: sub.state,
            });
        }
    }
    rows
}

type OnAction = Rc<dyn Fn(SideAction, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct Sidebar {
    rows: Rc<[SideRow]>,
    on_action: OnAction,
}

impl Sidebar {
    pub fn new(
        rows: Vec<SideRow>,
        on_action: impl Fn(SideAction, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            rows: rows.into(),
            on_action: Rc::new(on_action),
        }
    }
}

/// The room the hover buttons take, so a name is cut short instead of covered.
const ROW_ACTIONS_W: f32 = 160.0;
const REMOTE_ACTIONS_W: f32 = 100.0;
/// Worktree status dot geometry from design tokens (8 px circle, 4 px radius).
const WORKTREE_DOT_SIZE: f32 = SPACE_2;
const WORKTREE_DOT_RADIUS: f32 = RADIUS_SM;

/// The shared look of a sidebar row: focusable, with a hover group so its
/// actions can appear.
fn base_row(
    id: ElementId,
    window: &mut Window,
    cx: &mut App,
    ring: gpui::Hsla,
) -> gpui::Stateful<gpui::Div> {
    let handle = focus::handle(&id, window, cx);
    div()
        .id(id)
        .track_focus(&handle)
        .tab_stop(true)
        .group("side-row")
        .relative()
        .w_full()
        .flex()
        .items_center()
        .gap(px(SPACE_2))
        .h(px(ROW_LIST))
        .px(px(SPACE_2))
        .rounded(px(RADIUS_MD))
        .text_style(&TEXT_BODY, cx)
        .focus_visible(move |style| focus::ring(style, ring))
}

/// The strip of actions at the right end of a row, drawn over the row's text
/// with the hover colour behind it.
fn overlay(_background: gpui::Hsla) -> gpui::Div {
    div()
        .absolute()
        .left(px(0.))
        .right(px(0.))
        .top(px(0.))
        .h(px(ROW_LIST))
        .flex()
        .items_center()
        .justify_end()
}

/// A small text button that shows only while its row is hovered.
fn row_action(
    id: ElementId,
    label: &'static str,
    danger: bool,
    cx: &App,
    background: gpui::Hsla,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let p = cx.palette();
    div()
        .id(id)
        .flex_none()
        .px(px(SPACE_1 + 2.0))
        .h_full()
        .flex()
        .items_center()
        .bg(background)
        .text_style(&TEXT_SMALL, cx)
        .text_color(hsla(if danger { p.danger } else { p.accent }))
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            on_click(window, cx)
        })
        .child(label)
}

impl RenderOnce for Sidebar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let scroll = window
            .use_keyed_state("sidebar-scroll", cx, |_, _| UniformListScrollHandle::new())
            .read(cx)
            .clone();
        let count = self.rows.len();
        let rows = self.rows.clone();
        let on_action = self.on_action;
        let list = uniform_list("sidebar", count, move |range, window, cx| {
            let p = cx.palette();
            let ring = focus::ring_color(cx);
            range
                .map(|index| {
                    let hover = hsla(p.bg_hover);
                    match &rows[index] {
                        SideRow::Header(title) => div()
                            .flex()
                            .items_center()
                            .h(px(ROW_LIST))
                            .px(px(SPACE_2))
                            .text_style(&TEXT_LABEL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(title.clone())
                            .into_any_element(),
                        SideRow::Local {
                            name,
                            current,
                            tracking,
                        } => {
                            let (switch, rename, delete, open) = (
                                on_action.clone(),
                                on_action.clone(),
                                on_action.clone(),
                                on_action.clone(),
                            );
                            let (n1, n2, n3, n4) =
                                (name.clone(), name.clone(), name.clone(), name.clone());
                            let dot = div()
                                .flex_none()
                                .size(px(8.))
                                .rounded(px(4.))
                                .bg(hsla(if *current { p.accent } else { p.line_control }));
                            let mut row =
                                base_row(ElementId::from(("side-local", index)), window, cx, ring)
                                    .cursor_pointer()
                                    .hover(move |style| style.bg(hover))
                                    .on_click(move |event, window, cx| {
                                        if event.click_count() >= 2 {
                                            open(SideAction::Switch(n4.clone()), window, cx)
                                        }
                                    })
                                    .child(dot)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .truncate()
                                            .group_hover("side-row", |style| {
                                                style.pr(px(ROW_ACTIONS_W))
                                            })
                                            .font_weight(FontWeight(if *current {
                                                600.0
                                            } else {
                                                400.0
                                            }))
                                            .text_color(hsla(p.ink))
                                            .child(name.clone()),
                                    );
                            if let Some(text) = tracking {
                                row = row.child(
                                    div()
                                        .flex_none()
                                        .text_style(&TEXT_SMALL, cx)
                                        .text_color(hsla(p.ink_faint))
                                        .child(text.clone()),
                                );
                            }
                            let actions = overlay(hover)
                                .gap(px(SPACE_1))
                                .opacity(0.)
                                .group_hover("side-row", |style| style.opacity(1.))
                                .children((!*current).then(|| {
                                    row_action(
                                        ElementId::from(("side-switch", index)),
                                        "Switch",
                                        false,
                                        cx,
                                        hover,
                                        move |w, c| switch(SideAction::Switch(n1.clone()), w, c),
                                    )
                                }))
                                .child(row_action(
                                    ElementId::from(("side-rename", index)),
                                    "Rename",
                                    false,
                                    cx,
                                    hover,
                                    move |w, c| rename(SideAction::Rename(n2.clone()), w, c),
                                ))
                                .children((!*current).then(|| {
                                    row_action(
                                        ElementId::from(("side-delete", index)),
                                        "Delete",
                                        true,
                                        cx,
                                        hover,
                                        move |w, c| delete(SideAction::Delete(n3.clone()), w, c),
                                    )
                                }));
                            row.child(actions).into_any_element()
                        }
                        SideRow::Remote(name) => {
                            let checkout = on_action.clone();
                            let n = name.clone();
                            let actions = overlay(hover)
                                .opacity(0.)
                                .group_hover("side-row", |style| style.opacity(1.))
                                .child(row_action(
                                    ElementId::from(("side-checkout", index)),
                                    "Check out",
                                    false,
                                    cx,
                                    hover,
                                    move |w, c| {
                                        checkout(SideAction::CheckOutRemote(n.clone()), w, c)
                                    },
                                ));
                            base_row(ElementId::from(("side-remote", index)), window, cx, ring)
                                .hover(move |style| style.bg(hover))
                                .text_color(hsla(p.ink_muted))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .group_hover("side-row", |style| {
                                            style.pr(px(REMOTE_ACTIONS_W))
                                        })
                                        .child(name.clone()),
                                )
                                .child(actions)
                                .into_any_element()
                        }
                        SideRow::Tag(name) => {
                            let on_action = on_action.clone();
                            let tag = name.clone();
                            base_row(ElementId::from(("side-tag", index)), window, cx, ring)
                                .hover(move |style| style.bg(hover))
                                .cursor_pointer()
                                .on_click(move |_, window, cx| {
                                    on_action(SideAction::ManageTag(tag.clone()), window, cx)
                                })
                                .text_color(hsla(p.ink_muted))
                                .child(div().flex_1().min_w(px(0.)).truncate().child(name.clone()))
                                .into_any_element()
                        }
                        SideRow::Stash { index: at, label } => {
                            let (apply, pop, drop) =
                                (on_action.clone(), on_action.clone(), on_action.clone());
                            let at = *at;
                            let actions = overlay(hover)
                                .gap(px(SPACE_1))
                                .opacity(0.)
                                .group_hover("side-row", |style| style.opacity(1.))
                                .child(row_action(
                                    ElementId::from(("side-apply", index)),
                                    "Apply",
                                    false,
                                    cx,
                                    hover,
                                    move |w, c| apply(SideAction::StashApply(at), w, c),
                                ))
                                .child(row_action(
                                    ElementId::from(("side-pop", index)),
                                    "Pop",
                                    false,
                                    cx,
                                    hover,
                                    move |w, c| pop(SideAction::StashPop(at), w, c),
                                ))
                                .child(row_action(
                                    ElementId::from(("side-drop", index)),
                                    "Drop",
                                    true,
                                    cx,
                                    hover,
                                    move |w, c| drop(SideAction::StashDrop(at), w, c),
                                ));
                            base_row(ElementId::from(("side-stash", index)), window, cx, ring)
                                .hover(move |style| style.bg(hover))
                                .text_color(hsla(p.ink_muted))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .group_hover("side-row", |style| {
                                            style.pr(px(ROW_ACTIONS_W))
                                        })
                                        .child(label.clone()),
                                )
                                .child(actions)
                                .into_any_element()
                        }
                        SideRow::Worktree {
                            path,
                            label,
                            branch,
                            main,
                            current,
                            locked,
                            prunable,
                        } => {
                            let (open_click, open_btn, remove_btn) =
                                (on_action.clone(), on_action.clone(), on_action.clone());
                            let (p1, p2, p3) = (path.clone(), path.clone(), path.clone());
                            let can_open = !*current && !*prunable;
                            let can_remove = !*main && !*current;
                            let dot = div()
                                .flex_none()
                                .size(px(WORKTREE_DOT_SIZE))
                                .rounded(px(WORKTREE_DOT_RADIUS))
                                .bg(hsla(
                                if *current {
                                    p.accent
                                } else if *prunable || *locked {
                                    p.warning
                                } else {
                                    p.line_control
                                },
                            ));
                            let mut row = base_row(
                                ElementId::from(("side-worktree", index)),
                                window,
                                cx,
                                ring,
                            )
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .on_click(move |event, window, cx| {
                                if event.click_count() >= 2 && can_open {
                                    open_click(SideAction::WorktreeOpen(p1.clone()), window, cx)
                                }
                            })
                            .child(dot)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .group_hover("side-row", |style| style.pr(px(ROW_ACTIONS_W)))
                                    .font_weight(FontWeight(if *current { 600.0 } else { 400.0 }))
                                    .text_color(hsla(if *prunable { p.ink_muted } else { p.ink }))
                                    .child(label.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_style(&TEXT_SMALL, cx)
                                    .text_color(hsla(if *prunable || *locked {
                                        p.warning
                                    } else {
                                        p.ink_faint
                                    }))
                                    .child(branch.clone()),
                            );
                            if can_open || can_remove {
                                let actions = overlay(hover)
                                    .gap(px(SPACE_1))
                                    .opacity(0.)
                                    .group_hover("side-row", |style| style.opacity(1.))
                                    .children(can_open.then(|| {
                                        row_action(
                                            ElementId::from(("side-wt-open", index)),
                                            "Open",
                                            false,
                                            cx,
                                            hover,
                                            move |w, c| {
                                                open_btn(SideAction::WorktreeOpen(p2.clone()), w, c)
                                            },
                                        )
                                    }))
                                    .children(can_remove.then(|| {
                                        row_action(
                                            ElementId::from(("side-wt-remove", index)),
                                            "Remove",
                                            true,
                                            cx,
                                            hover,
                                            move |w, c| {
                                                remove_btn(
                                                    SideAction::WorktreeRemove(p3.clone()),
                                                    w,
                                                    c,
                                                )
                                            },
                                        )
                                    }));
                                row = row.child(actions);
                            }
                            row.into_any_element()
                        }
                        SideRow::Submodule {
                            path,
                            name: _,
                            status,
                            state,
                        } => {
                            let (update_btn, sync_btn) = (on_action.clone(), on_action.clone());
                            let (p1, p2) = (path.clone(), path.clone());
                            let status_color = match state {
                                SubmoduleState::Clean => p.ink_faint,
                                SubmoduleState::MergeConflict => p.danger,
                                SubmoduleState::Uninitialized
                                | SubmoduleState::OutOfSync
                                | SubmoduleState::Dirty => p.warning,
                            };
                            let actions = overlay(hover)
                                .gap(px(SPACE_1))
                                .opacity(0.)
                                .group_hover("side-row", |style| style.opacity(1.))
                                .child(row_action(
                                    ElementId::from(("side-sub-update", index)),
                                    "Update",
                                    false,
                                    cx,
                                    hover,
                                    move |w, c| {
                                        update_btn(
                                            SideAction::SubmoduleUpdate(Some(p1.clone())),
                                            w,
                                            c,
                                        )
                                    },
                                ))
                                .child(row_action(
                                    ElementId::from(("side-sub-sync", index)),
                                    "Sync",
                                    false,
                                    cx,
                                    hover,
                                    move |w, c| {
                                        sync_btn(SideAction::SubmoduleSync(Some(p2.clone())), w, c)
                                    },
                                ));
                            base_row(ElementId::from(("side-submodule", index)), window, cx, ring)
                                .hover(move |style| style.bg(hover))
                                .text_color(hsla(p.ink_muted))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .truncate()
                                        .group_hover("side-row", |style| {
                                            style.pr(px(ROW_ACTIONS_W))
                                        })
                                        .child(path.clone()),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_style(&TEXT_SMALL, cx)
                                        .text_color(hsla(status_color))
                                        .child(status.clone()),
                                )
                                .child(actions)
                                .into_any_element()
                        }
                    }
                })
                .collect()
        })
        .track_scroll(&scroll)
        .flex_1()
        .h_full();
        div()
            .flex()
            .flex_1()
            .min_h(px(0.))
            .px(px(SPACE_2))
            .child(list)
            .child(Scrollbar::new("sidebar-scrollbar", scroll, count, ROW_LIST))
    }
}

#[cfg(test)]
mod tests {
    use invoke_core::{BranchName, Oid, RefName, Upstream};

    use super::*;

    fn oid(c: char) -> Oid {
        Oid::from_hex(&c.to_string().repeat(40)).unwrap()
    }

    fn refs(names: &[&str]) -> RefsSnapshot {
        let mut snapshot = RefsSnapshot::default();
        for name in names {
            snapshot.refs.insert(RefName((*name).to_owned()), oid('a'));
        }
        snapshot
    }

    fn head(name: &str) -> Head {
        Head::Branch {
            name: BranchName(name.into()),
            oid: oid('a'),
        }
    }

    fn texts(rows: &[SideRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                SideRow::Header(t) => format!("# {t}"),
                SideRow::Local { name, current, .. } => {
                    format!("{}{name}", if *current { "* " } else { "" })
                }
                SideRow::Remote(n) => format!("r {n}"),
                SideRow::Tag(n) => format!("t {n}"),
                SideRow::Stash { index, label } => format!("s{index} {label}"),
                SideRow::Worktree {
                    label,
                    branch,
                    current,
                    ..
                } => format!("w {}{label} ({branch})", if *current { "* " } else { "" }),
                SideRow::Submodule { path, status, .. } => format!("m {path} [{status}]"),
            })
            .collect()
    }

    #[test]
    fn the_current_branch_comes_first_then_the_rest_by_name() {
        let snapshot = refs(&[
            "refs/heads/zeta",
            "refs/heads/main",
            "refs/heads/alpha",
            "refs/remotes/origin/main",
            "refs/remotes/origin/HEAD",
            "refs/remotes/up/dev",
            "refs/tags/v1",
        ]);
        let rows = sidebar_rows(&snapshot, &head("main"), &[], &AdvancedSnapshot::default());
        assert_eq!(
            texts(&rows),
            vec![
                "# LOCAL",
                "* main",
                "alpha",
                "zeta",
                "# REMOTE · ORIGIN",
                "r origin/main",
                "# REMOTE · UP",
                "r up/dev",
                "# TAGS",
                "t v1",
            ]
        );
    }

    #[test]
    fn an_unborn_branch_is_listed_as_the_current_one() {
        let rows = sidebar_rows(
            &RefsSnapshot::default(),
            &Head::Unborn(BranchName("main".into())),
            &[],
            &AdvancedSnapshot::default(),
        );
        assert_eq!(texts(&rows), vec!["# LOCAL", "* main"]);
    }

    #[test]
    fn tracking_reads_like_the_header() {
        let mut snapshot = refs(&["refs/heads/main"]);
        snapshot.upstreams.insert(
            BranchName("main".into()),
            Upstream {
                name: RefName("refs/remotes/origin/main".into()),
                tracking: Tracking::Apart {
                    ahead: 2,
                    behind: 0,
                },
            },
        );
        let rows = sidebar_rows(&snapshot, &head("main"), &[], &AdvancedSnapshot::default());
        assert!(matches!(
            &rows[1],
            SideRow::Local { tracking: Some(t), .. } if t == "↑2 ↓0"
        ));
    }

    #[test]
    fn a_detached_head_has_no_current_branch() {
        let snapshot = refs(&["refs/heads/main"]);
        let rows = sidebar_rows(
            &snapshot,
            &Head::Detached(oid('a')),
            &[],
            &AdvancedSnapshot::default(),
        );
        assert!(matches!(&rows[1], SideRow::Local { current: false, .. }));
    }

    #[test]
    fn empty_sections_have_no_rows() {
        assert!(
            sidebar_rows(
                &RefsSnapshot::default(),
                &Head::Detached(oid('a')),
                &[],
                &AdvancedSnapshot::default(),
            )
            .is_empty()
        );
    }

    #[test]
    fn worktrees_and_submodules_sections_appear_when_present() {
        use invoke_core::{RepoPath, SubmoduleEntry, WorktreeEntry};
        let adv = AdvancedSnapshot {
            worktrees: vec![
                WorktreeEntry {
                    path: "/r".into(),
                    name: "r".into(),
                    head: Some(head("main")),
                    index: None,
                    is_main: true,
                    is_current: true,
                    locked: None,
                    prunable: false,
                },
                WorktreeEntry {
                    path: "/r-feat".into(),
                    name: "r-feat".into(),
                    head: Some(head("feat")),
                    index: None,
                    is_main: false,
                    is_current: false,
                    locked: Some("pinned".into()),
                    prunable: false,
                },
            ],
            submodules: vec![SubmoduleEntry {
                name: "libs/a".into(),
                path: RepoPath("libs/a".into()),
                url: None,
                index_oid: Some(oid('a')),
                head_oid: None,
                head: None,
                state: SubmoduleState::Uninitialized,
            }],
            lfs: Default::default(),
        };
        let rows = sidebar_rows(
            &RefsSnapshot::default(),
            &Head::Detached(oid('a')),
            &[],
            &adv,
        );
        assert_eq!(
            texts(&rows),
            vec![
                "# WORKTREES",
                "w * r (main)",
                "w r-feat (feat · locked)",
                "# SUBMODULES",
                "m libs/a [uninit]",
            ]
        );
    }

    #[test]
    fn worktree_dot_uses_design_tokens() {
        assert_eq!(WORKTREE_DOT_SIZE, SPACE_2);
        assert_eq!(WORKTREE_DOT_RADIUS, RADIUS_SM);
    }
}
