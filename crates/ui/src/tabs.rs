//! The repo tab bar: one tab per open repository, a "New tab" button, and
//! the Dark / Light theme switch.

use std::rc::Rc;

use gpui::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce, Role, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use tokens::{
    RADIUS_MD, RADIUS_SM, ROW_DIFF, ROW_LIST, SIDEBAR, SPACE_1, SPACE_2, SPACE_3, TEXT_BODY_MEDIUM,
    TEXT_SMALL, TOOLBAR, Theme,
};

use crate::components::{Button, IconName, text_tooltip};
use crate::convert::hsla;
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::{ThemeExt, set_theme};

/// What one tab shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabItem {
    /// The repo's folder name.
    pub title: SharedString,
    pub path: SharedString,
    /// A short state hint after the title, such as the number of changed
    /// files. Empty for none.
    pub badge: SharedString,
}

type OnIndex = Rc<dyn Fn(usize, &mut Window, &mut App)>;
type OnNew = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct TabBar {
    items: Vec<TabItem>,
    active: Option<usize>,
    on_select: OnIndex,
    on_close: OnIndex,
    on_new: OnNew,
    on_palette: Option<OnNew>,
}

impl TabBar {
    /// `active` is `None` while the welcome screen is shown.
    pub fn new(
        items: Vec<TabItem>,
        active: Option<usize>,
        on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
        on_close: impl Fn(usize, &mut Window, &mut App) + 'static,
        on_new: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            items,
            active,
            on_select: Rc::new(on_select),
            on_close: Rc::new(on_close),
            on_new: Rc::new(on_new),
            on_palette: None,
        }
    }

    /// Adds a compact Command Palette trigger (`Ctrl+K`) on the right of the tab bar.
    pub fn on_palette(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_palette = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for TabBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let current_theme = cx.ui_theme().theme();
        let (next_theme, theme_label, theme_icon) = match current_theme {
            Theme::Dark => (Theme::Light, "Light", IconName::Sun),
            Theme::Light => (Theme::Dark, "Dark", IconName::Moon),
        };

        let scroll = window
            .use_keyed_state("tab-bar-scroll", cx, |_, _| ScrollHandle::new())
            .read(cx)
            .clone();
        let last_active = window.use_keyed_state("tab-bar-last-active", cx, |_, _| None::<usize>);
        if *last_active.read(cx) != self.active {
            last_active.update(cx, |slot, _| *slot = self.active);
            if let Some(index) = self.active {
                scroll.scroll_to_item(index);
            }
        }
        let last_focused = window.use_keyed_state("tab-bar-last-focused", cx, |_, _| None::<usize>);
        let mut focused_tab = None;

        let mut strip = div()
            .id("tab-strip")
            .flex()
            .flex_1()
            .min_w(px(0.))
            .items_center()
            .gap(px(SPACE_1))
            .overflow_x_scroll()
            .track_scroll(&scroll);

        for (index, item) in self.items.into_iter().enumerate() {
            let active = self.active == Some(index);
            let id = ElementId::from(("tab", index));
            let handle = focus::handle(&id, window, cx);
            let close_id = ElementId::from(("tab-close", index));
            let close_handle = focus::handle(&close_id, window, cx);
            if handle.is_focused(window) || close_handle.is_focused(window) {
                focused_tab = Some(index);
            }
            let on_select = self.on_select.clone();
            let on_close = self.on_close.clone();
            let hover_fill = hsla(p.bg_hover);
            let hover_ink = hsla(p.ink);
            let close_hover_bg = hsla(if active { p.bg_hover } else { p.bg_raised });

            let close_label: SharedString = format!("Close {}", item.title).into();
            let path = item.path;
            let mut tab = div()
                .id(id)
                .role(Role::Tab)
                .aria_selected(active)
                .aria_label(item.title.clone())
                .tooltip(move |_, cx| text_tooltip(path.clone(), cx))
                .track_focus(&handle)
                .tab_stop(true)
                .relative()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(SPACE_2))
                .h(px(ROW_LIST))
                .max_w(px(SIDEBAR))
                .pl(px(SPACE_3))
                .pr(px(SPACE_1))
                .rounded(px(RADIUS_MD))
                .text_style(&TEXT_BODY_MEDIUM, cx)
                .text_color(hsla(if active { p.ink } else { p.ink_muted }))
                .cursor_pointer()
                .focus_visible(move |style| focus::inset_ring(style, ring))
                .on_click(move |_, window, cx| on_select(index, window, cx))
                .child(div().min_w(px(0.)).truncate().child(item.title));

            tab = if active {
                tab.bg(hsla(p.bg_panel))
                    .border_1()
                    .border_color(hsla(p.line))
                    .child(
                        div()
                            .absolute()
                            .left(px(SPACE_2))
                            .right(px(SPACE_2))
                            .bottom(px(0.))
                            .h(px(focus::ACCENT_BAR_W))
                            .rounded(px(RADIUS_SM))
                            .bg(hsla(p.accent)),
                    )
            } else {
                tab.hover(move |style| style.bg(hover_fill).text_color(hover_ink))
            };

            if !item.badge.is_empty() {
                tab = tab.child(
                    div()
                        .flex_none()
                        .px(px(SPACE_1))
                        .rounded(px(RADIUS_SM))
                        .bg(hsla(if active { p.accent_soft } else { p.bg_raised }))
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(if active { p.accent } else { p.ink_muted }))
                        .child(item.badge),
                );
            }
            let close = div()
                .id(close_id)
                .role(Role::Button)
                .aria_label(close_label.clone())
                .tooltip(move |_, cx| text_tooltip(close_label.clone(), cx))
                .track_focus(&close_handle)
                .tab_stop(true)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(ROW_DIFF))
                .rounded(px(RADIUS_SM))
                .text_color(hsla(p.ink_muted))
                .cursor_pointer()
                .hover(move |style| style.bg(close_hover_bg).text_color(hover_ink))
                .focus_visible(move |style| focus::inset_ring(style, ring))
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_close(index, window, cx);
                })
                .child("×");
            strip = strip.child(tab.child(close));
        }

        if *last_focused.read(cx) != focused_tab {
            last_focused.update(cx, |slot, _| *slot = focused_tab);
            if let Some(index) = focused_tab {
                scroll.scroll_to_item(index);
            }
        }

        let on_new = self.on_new;
        let show_new = self.active.is_some();
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(SPACE_2))
            .h(px(TOOLBAR))
            .w_full()
            .px(px(SPACE_3))
            .bg(hsla(p.bg_app))
            .border_b_1()
            .border_color(hsla(p.line))
            .child(strip)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(SPACE_1))
                    .children(show_new.then(|| {
                        Button::new("tab-new", "New tab")
                            .icon(IconName::Plus)
                            .ghost()
                            .compact()
                            .on_click(move |_, window, cx| on_new(window, cx))
                    }))
                    .children(self.on_palette.map(|open_palette| {
                        Button::new("tab-palette", "Commands")
                            .icon(IconName::Command)
                            .shortcut("Ctrl+K")
                            .ghost()
                            .compact()
                            .on_click(move |_, window, cx| open_palette(window, cx))
                    }))
                    .child(
                        Button::new("theme-toggle", theme_label)
                            .icon(theme_icon)
                            .ghost()
                            .compact()
                            .on_click(move |_, _, cx| set_theme(next_theme, cx)),
                    ),
            )
    }
}
