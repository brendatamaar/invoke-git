//! Quick Command Palette (`Ctrl+K`): keyboard-first switcher for repositories,
//! branches, views, and common Git actions without crowding toolbars.

use std::path::PathBuf;

use gpui::{
    AppContext, Context, ElementId, Entity, EventEmitter, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, deferred, div, px,
};
use tokens::{
    DIALOG, RADIUS_LG, RADIUS_MD, RADIUS_SM, ROW_LIST, Rgba, SPACE_1, SPACE_2, SPACE_3, SPACE_4,
    SPACE_8, TEXT_BODY_MEDIUM, TEXT_SMALL,
};
use ui::convert::{hsla, shadows};
use ui::focus;
use ui::{EditorEvent, Icon, IconName, TextEditor, TextStyleExt, ThemeExt};

/// What running a palette entry asks the workspace or active repo to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    OpenRepo,
    InitRepo,
    CloneRepo,
    OpenRecent(PathBuf),
    ShowWorkingTree,
    ShowHistory,
    ShowPullRequests,
    ShowReflog,
    ToggleSearch,
    SwitchBranch(String),
    NewBranch,
    StashChanges,
    NewTag,
    Fetch,
    Pull,
    ReviewAndPush,
    Refresh,
    Undo,
    ToggleTheme,
}

/// One searchable command entry shown in [`CommandPalette`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteItem {
    pub title: SharedString,
    pub subtitle: Option<SharedString>,
    pub shortcut: Option<SharedString>,
    pub icon: IconName,
    pub action: PaletteAction,
}

/// Pure case-insensitive multi-term matcher and ranker for palette items.
///
/// Every whitespace-separated token in `query` must match either `title` or
/// `subtitle`. Exact/prefix matches on `title` rank ahead of substring matches
/// on `title`, which rank ahead of `subtitle`-only matches, preserving input
/// order on ties.
pub fn filter_items(items: &[PaletteItem], query: &str) -> Vec<usize> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return (0..items.len()).collect();
    }
    let full = tokens.join(" ");
    let mut ranked: Vec<(u8, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let title = item.title.to_lowercase();
            let subtitle = item
                .subtitle
                .as_ref()
                .map(|s| s.to_lowercase())
                .unwrap_or_default();
            let all_match = tokens
                .iter()
                .all(|tok| title.contains(tok) || subtitle.contains(tok));
            if !all_match {
                return None;
            }
            let score = if title == full {
                0
            } else if title.starts_with(&full) {
                1
            } else if tokens.iter().all(|tok| title.contains(tok)) {
                2
            } else {
                3
            };
            Some((score, index))
        })
        .collect();
    ranked.sort_by_key(|&(score, index)| (score, index));
    ranked.into_iter().map(|(_, index)| index).collect()
}

pub enum PaletteEvent {
    Run(PaletteAction),
    Closed,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

pub struct CommandPalette {
    open: bool,
    items: Vec<PaletteItem>,
    filtered: Vec<usize>,
    selected: usize,
    editor: Entity<TextEditor>,
    _sub: Subscription,
}

impl CommandPalette {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| TextEditor::new("Type a command or branch…", 1, cx).single_line());
        let sub = cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| match event {
            EditorEvent::Changed => this.refilter(cx),
            EditorEvent::Submit => this.run_selected(cx),
        });
        Self {
            open: false,
            items: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            editor,
            _sub: sub,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, items: Vec<PaletteItem>, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.items = items;
        self.editor.update(cx, |ed, cx| ed.set_text("", cx));
        self.filtered = (0..self.items.len()).collect();
        self.selected = 0;
        self.editor.update(cx, |ed, cx| ed.focus(window, cx));
        cx.notify();
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.items.clear();
        self.filtered.clear();
        self.selected = 0;
        cx.emit(PaletteEvent::Closed);
        cx.notify();
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.editor.read(cx).text().to_owned();
        self.filtered = filter_items(&self.items, &query);
        self.selected = 0;
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.filtered.len();
        if count == 0 {
            return;
        }
        let next = (self.selected as isize + delta).rem_euclid(count as isize) as usize;
        self.selected = next;
        cx.notify();
    }

    fn run_at(&mut self, filtered_index: usize, cx: &mut Context<Self>) {
        let Some(&item_index) = self.filtered.get(filtered_index) else {
            return;
        };
        let Some(action) = self.items.get(item_index).map(|item| item.action.clone()) else {
            return;
        };
        self.open = false;
        self.items.clear();
        self.filtered.clear();
        self.selected = 0;
        cx.emit(PaletteEvent::Run(action));
        cx.notify();
    }

    fn run_selected(&mut self, cx: &mut Context<Self>) {
        self.run_at(self.selected, cx);
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let p = cx.palette();
        let backdrop_close = cx.entity();
        let key_entity = cx.entity();

        let mut list = div()
            .id("palette-results")
            .flex()
            .flex_col()
            .max_h(px(ROW_LIST * 10.0))
            .overflow_y_scroll()
            .p(px(SPACE_2))
            .gap(px(SPACE_1 / 2.0));

        if self.filtered.is_empty() {
            list = list.child(
                div()
                    .px(px(SPACE_3))
                    .py(px(SPACE_4))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .text_center()
                    .child("No matching commands."),
            );
        } else {
            for (pos, &item_idx) in self.filtered.iter().enumerate() {
                let item = &self.items[item_idx];
                let selected = pos == self.selected;
                let run_entity = cx.entity();
                let hover = hsla(p.bg_hover);
                let mut row = div()
                    .id(ElementId::from(("palette-item", pos)))
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(SPACE_2))
                    .min_h(px(ROW_LIST + SPACE_1))
                    .px(px(SPACE_3))
                    .py(px(SPACE_1))
                    .rounded(px(RADIUS_MD))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        run_entity.update(cx, |this, cx| this.run_at(pos, cx));
                    })
                    .child(Icon::new(item.icon).small().color(if selected {
                        p.accent
                    } else {
                        p.ink_muted
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .items_center()
                            .gap(px(SPACE_2))
                            .child(
                                div()
                                    .flex_none()
                                    .text_style(&TEXT_BODY_MEDIUM, cx)
                                    .text_color(hsla(p.ink))
                                    .child(item.title.clone()),
                            )
                            .children(item.subtitle.clone().map(|sub| {
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_style(&TEXT_SMALL, cx)
                                    .text_color(hsla(p.ink_muted))
                                    .child(sub)
                            })),
                    )
                    .children(item.shortcut.clone().map(|key| {
                        div()
                            .flex_none()
                            .px(px(SPACE_2))
                            .py(px(SPACE_1 / 2.0))
                            .rounded(px(RADIUS_SM))
                            .bg(hsla(p.bg_app))
                            .border_1()
                            .border_color(hsla(p.line))
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(key)
                    }));
                row = if selected {
                    row.bg(hsla(p.bg_raised)).child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .top(px(SPACE_1))
                            .bottom(px(SPACE_1))
                            .w(px(focus::ACCENT_BAR_W))
                            .rounded(px(RADIUS_SM))
                            .bg(hsla(p.accent)),
                    )
                } else {
                    row.hover(move |s| s.bg(hover))
                };
                list = list.child(row);
            }
        }

        let card = div()
            .id("command-palette-card")
            .occlude()
            .flex()
            .flex_col()
            .w(px(DIALOG))
            .bg(hsla(p.bg_panel))
            .border_1()
            .border_color(hsla(p.line))
            .rounded(px(RADIUS_LG))
            .shadow(shadows(p.shadow_overlay))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(
                move |event: &KeyDownEvent, _, cx| match event.keystroke.key.as_str() {
                    "escape" => {
                        cx.stop_propagation();
                        key_entity.update(cx, |this, cx| this.close(cx));
                    }
                    "up" => {
                        cx.stop_propagation();
                        key_entity.update(cx, |this, cx| this.step(-1, cx));
                    }
                    "down" => {
                        cx.stop_propagation();
                        key_entity.update(cx, |this, cx| this.step(1, cx));
                    }
                    "enter" => {
                        cx.stop_propagation();
                        key_entity.update(cx, |this, cx| this.run_selected(cx));
                    }
                    _ => {}
                },
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(SPACE_2))
                    .p(px(SPACE_3))
                    .border_b_1()
                    .border_color(hsla(p.line))
                    .child(Icon::new(IconName::Search).color(p.ink_muted))
                    .child(div().flex_1().min_w(px(0.)).child(self.editor.clone())),
            )
            .child(list)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(SPACE_3))
                    .py(px(SPACE_2))
                    .border_t_1()
                    .border_color(hsla(p.line))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_faint))
                    .child("↑↓ navigate · Enter run · Esc close")
                    .child(format!("{} commands", self.filtered.len())),
            );

        deferred(
            div()
                .id("command-palette-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(SPACE_8 * 3.0))
                .bg(hsla(Rgba::new(0, 0, 0, 0.45)))
                .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                    backdrop_close.update(cx, |this, cx| this.close(cx));
                })
                .child(card),
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn sample_items() -> Vec<PaletteItem> {
        vec![
            PaletteItem {
                title: "Fetch".into(),
                subtitle: Some("Git".into()),
                shortcut: None,
                icon: IconName::Fetch,
                action: PaletteAction::Fetch,
            },
            PaletteItem {
                title: "Switch branch: main".into(),
                subtitle: Some("Branch".into()),
                shortcut: None,
                icon: IconName::Branch,
                action: PaletteAction::SwitchBranch("main".into()),
            },
            PaletteItem {
                title: "Pull Requests".into(),
                subtitle: Some("View".into()),
                shortcut: None,
                icon: IconName::PullRequest,
                action: PaletteAction::ShowPullRequests,
            },
            PaletteItem {
                title: "Pull".into(),
                subtitle: Some("Git".into()),
                shortcut: None,
                icon: IconName::Pull,
                action: PaletteAction::Pull,
            },
        ]
    }

    #[test]
    fn empty_query_returns_all_in_order() {
        let items = sample_items();
        assert_eq!(filter_items(&items, "   "), vec![0, 1, 2, 3]);
    }

    #[test]
    fn exact_and_prefix_matches_rank_ahead_of_substring_matches() {
        let items = sample_items();
        // "pull" matches "Pull" (exact, idx 3) ahead of "Pull Requests" (prefix, idx 2)
        assert_eq!(filter_items(&items, "pull"), vec![3, 2]);
        // multi-term across title and subtitle
        assert_eq!(filter_items(&items, "branch main"), vec![1]);
    }

    proptest! {
        #[test]
        fn filtered_indices_are_valid_unique_and_match_every_token(
            titles in prop::collection::vec("[a-zA-Z0-9 _:-]{1,24}", 0..20),
            query in "[a-zA-Z0-9 ]{0,16}",
        ) {
            let items: Vec<PaletteItem> = titles
                .into_iter()
                .map(|title| PaletteItem {
                    title: title.into(),
                    subtitle: Some("Action".into()),
                    shortcut: None,
                    icon: IconName::Command,
                    action: PaletteAction::Refresh,
                })
                .collect();
            let indices = filter_items(&items, &query);
            let mut seen = std::collections::HashSet::new();
            let tokens: Vec<String> = query
                .split_whitespace()
                .map(str::to_lowercase)
                .filter(|t| !t.is_empty())
                .collect();
            for &ix in &indices {
                prop_assert!(ix < items.len());
                prop_assert!(seen.insert(ix));
                let title = items[ix].title.to_lowercase();
                let sub = items[ix].subtitle.as_deref().unwrap_or("").to_lowercase();
                for tok in &tokens {
                    prop_assert!(title.contains(tok) || sub.contains(tok));
                }
            }
            if tokens.is_empty() {
                prop_assert_eq!(indices, (0..items.len()).collect::<Vec<_>>());
            }
        }
    }
}
