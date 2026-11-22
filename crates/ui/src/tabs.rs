//! The repo tab bar: one tab per open repository, plus a "New tab" button.

use std::rc::Rc;

use gpui::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use tokens::{
    RADIUS_MD, RADIUS_SM, ROW_LIST, SPACE_1, SPACE_2, SPACE_3, TEXT_BODY_MEDIUM, TEXT_SMALL,
    TOOLBAR,
};

use crate::components::Button;
use crate::convert::hsla;
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// What one tab shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabItem {
    /// The repo's folder name.
    pub title: SharedString,
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
        }
    }
}

impl RenderOnce for TabBar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let mut bar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(SPACE_1))
            .h(px(TOOLBAR))
            .w_full()
            .px(px(SPACE_3))
            .bg(hsla(p.bg_panel))
            .border_b_1()
            .border_color(hsla(p.line));

        for (index, item) in self.items.into_iter().enumerate() {
            let active = self.active == Some(index);
            let id = ElementId::from(("tab", index));
            let handle = focus::handle(&id, window, cx);
            let (fill, ink) = if active {
                (Some(p.bg_raised), p.ink)
            } else {
                (None, p.ink_muted)
            };
            let on_select = self.on_select.clone();
            let on_close = self.on_close.clone();
            let hover_fill = hsla(p.bg_hover);
            let mut tab = div()
                .id(id)
                .track_focus(&handle)
                .tab_stop(true)
                .flex()
                .flex_none()
                .items_center()
                .gap(px(SPACE_2))
                .h(px(ROW_LIST))
                .max_w(px(220.))
                .pl(px(SPACE_3))
                .pr(px(SPACE_1))
                .rounded(px(RADIUS_MD))
                .text_style(&TEXT_BODY_MEDIUM, cx)
                .text_color(hsla(ink))
                .cursor_pointer()
                .hover(move |style| style.bg(hover_fill))
                .focus_visible(move |style| focus::ring(style, ring))
                .on_click(move |_, window, cx| on_select(index, window, cx))
                .child(div().truncate().child(item.title));
            if let Some(fill) = fill {
                tab = tab.bg(hsla(fill));
            }
            if !item.badge.is_empty() {
                tab = tab.child(
                    div()
                        .flex_none()
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(item.badge),
                );
            }
            let close = div()
                .id(ElementId::from(("tab-close", index)))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(px(20.))
                .rounded(px(RADIUS_SM))
                .text_color(hsla(p.ink_muted))
                .cursor_pointer()
                .hover(move |style| style.bg(hsla(p.bg_hover)))
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_close(index, window, cx);
                })
                .child("×");
            bar = bar.child(tab.child(close));
        }

        let on_new = self.on_new;
        bar.child(
            Button::new("tab-new", "New tab")
                .ghost()
                .on_click(move |_, window, cx| on_new(window, cx)),
        )
    }
}
