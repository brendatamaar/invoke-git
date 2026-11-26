//! Primary underlined navigation tabs, visually distinct from `Segmented`
//! local toolbar toggles.

use std::rc::Rc;

use gpui::{
    App, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use tokens::{RADIUS_MD, RADIUS_SM, ROW_LIST, SPACE_1, SPACE_2, TEXT_BODY_MEDIUM, TEXT_SMALL};

use crate::components::{Icon, IconName};
use crate::convert::hsla;
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// One primary navigation tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NavTab {
    pub label: SharedString,
    pub icon: Option<IconName>,
    pub badge: Option<SharedString>,
}

impl NavTab {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            icon: None,
            badge: None,
        }
    }

    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn badge(mut self, badge: impl Into<SharedString>) -> Self {
        self.badge = Some(badge.into());
        self
    }
}

type OnSelect = Rc<dyn Fn(usize, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct NavTabs {
    id: SharedString,
    tabs: Vec<NavTab>,
    selected: usize,
    on_select: OnSelect,
}

impl NavTabs {
    pub fn new(
        id: impl Into<SharedString>,
        tabs: Vec<NavTab>,
        selected: usize,
        on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            tabs,
            selected,
            on_select: Rc::new(on_select),
        }
    }
}

impl RenderOnce for NavTabs {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let mut row = div().flex().flex_none().items_center().gap(px(SPACE_1));
        for (index, tab) in self.tabs.into_iter().enumerate() {
            let id = ElementId::from((self.id.clone(), index));
            let handle = focus::handle(&id, window, cx);
            let active = index == self.selected;
            let on_select = self.on_select.clone();
            let hover_bg = hsla(p.bg_hover);
            let hover_ink = hsla(p.ink);
            let text_color = if active { p.ink } else { p.ink_muted };
            let mut item = div()
                .id(id)
                .track_focus(&handle)
                .tab_stop(true)
                .relative()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(SPACE_1))
                .h(px(ROW_LIST))
                .px(px(SPACE_2))
                .rounded(px(RADIUS_MD))
                .whitespace_nowrap()
                .text_style(&TEXT_BODY_MEDIUM, cx)
                .font_weight(FontWeight(if active { 600.0 } else { 500.0 }))
                .text_color(hsla(text_color))
                .cursor_pointer()
                .focus_visible(move |style| focus::inset_ring(style, ring))
                .on_click(move |_, window, cx| on_select(index, window, cx))
                .children(tab.icon.map(|icon| {
                    Icon::new(icon)
                        .small()
                        .color(if active { p.accent } else { p.ink_muted })
                }))
                .child(tab.label);
            if let Some(badge) = tab.badge.filter(|b| !b.is_empty()) {
                item = item.child(
                    div()
                        .flex_none()
                        .px(px(SPACE_1))
                        .rounded(px(RADIUS_SM))
                        .bg(hsla(if active { p.accent_soft } else { p.bg_raised }))
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(if active { p.accent } else { p.ink_muted }))
                        .child(badge),
                );
            }
            item = if active {
                item.child(
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
                item.hover(move |style| style.bg(hover_bg).text_color(hover_ink))
            };
            row = row.child(item);
        }
        row
    }
}
