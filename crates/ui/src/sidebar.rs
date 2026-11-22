//! The repository sidebar (backlog task 15, and the stash section of task
//! 20): local branches with their ahead and behind counts, remote branches
//! and tags, in sections. The rows are pure (refs in, rows out); the
//! component draws them and reports what the user asked for.

use std::collections::BTreeSet;
use std::rc::Rc;

use gpui::{
    App, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window, div, px, uniform_list,
};
use invoke_core::{Head, RefsSnapshot, StashEntry, Tracking};
use tokens::{RADIUS_MD, ROW_LIST, SPACE_1, SPACE_2, TEXT_BODY, TEXT_LABEL, TEXT_SMALL};

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
    /// Apply, pop or drop the stash entry at this position.
    StashApply(usize),
    StashPop(usize),
    StashDrop(usize),
}

fn tracking_text(tracking: Tracking) -> String {
    match tracking {
        Tracking::Gone => "gone".to_owned(),
        Tracking::Apart { ahead, behind } => format!("↑{ahead} ↓{behind}"),
    }
}

/// The rows for the refs: LOCAL (the current branch first, then the others
/// by name), one REMOTE section per remote, TAGS, and STASHES when there are
/// any. A section without entries has no rows.
pub fn sidebar_rows(refs: &RefsSnapshot, head: &Head, stashes: &[StashEntry]) -> Vec<SideRow> {
    let current = match head {
        Head::Branch { name, .. } | Head::Unborn(name) => Some(name.0.as_str()),
        Head::Detached(_) => None,
    };
    let mut locals: Vec<(&str, Option<String>)> = Vec::new();
    let mut remotes: Vec<(&str, &str)> = Vec::new(); // (remote, branch)
    let mut tags: BTreeSet<&str> = BTreeSet::new();
    for name in refs.refs.keys() {
        let full = name.0.as_str();
        if let Some(branch) = full.strip_prefix("refs/heads/") {
            let tracking = refs
                .upstreams
                .get(&invoke_core::BranchName(branch.to_owned()))
                .map(|upstream| tracking_text(upstream.tracking));
            locals.push((branch, tracking));
        } else if let Some(rest) = full.strip_prefix("refs/remotes/") {
            if let Some((remote, branch)) = rest.split_once('/')
                && branch != "HEAD"
            {
                remotes.push((remote, branch));
            }
        } else if let Some(tag) = full.strip_prefix("refs/tags/") {
            tags.insert(tag);
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
    if !tags.is_empty() {
        rows.push(SideRow::Header("TAGS".to_owned()));
        rows.extend(tags.into_iter().map(|t| SideRow::Tag(t.to_owned())));
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
                                .child(div().flex_1().min_w(px(0.)).truncate().child(name.clone()))
                                .child(actions)
                                .into_any_element()
                        }
                        SideRow::Tag(name) => {
                            base_row(ElementId::from(("side-tag", index)), window, cx, ring)
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
                                .child(div().flex_1().min_w(px(0.)).truncate().child(label.clone()))
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
        let rows = sidebar_rows(&snapshot, &head("main"), &[]);
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
        let rows = sidebar_rows(&snapshot, &head("main"), &[]);
        assert!(matches!(
            &rows[1],
            SideRow::Local { tracking: Some(t), .. } if t == "↑2 ↓0"
        ));
    }

    #[test]
    fn a_detached_head_has_no_current_branch() {
        let snapshot = refs(&["refs/heads/main"]);
        let rows = sidebar_rows(&snapshot, &Head::Detached(oid('a')), &[]);
        assert!(matches!(&rows[1], SideRow::Local { current: false, .. }));
    }

    #[test]
    fn empty_sections_have_no_rows() {
        assert!(sidebar_rows(&RefsSnapshot::default(), &Head::Detached(oid('a')), &[]).is_empty());
    }
}
