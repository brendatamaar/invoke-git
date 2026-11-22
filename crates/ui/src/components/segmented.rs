use std::rc::Rc;

use gpui::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use tokens::{RADIUS_MD, RADIUS_SM, SPACE_1, SPACE_3, TEXT_SMALL};

use crate::convert::hsla;
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

type OnSelect = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// A group of mutually exclusive choices (the Split and Unified switch): a
/// `bg-app` well with the chosen option raised.
#[derive(IntoElement)]
pub struct Segmented {
    id: SharedString,
    options: Vec<SharedString>,
    selected: usize,
    on_select: OnSelect,
}

impl Segmented {
    pub fn new(
        id: impl Into<SharedString>,
        options: Vec<SharedString>,
        selected: usize,
        on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            options,
            selected,
            on_select: Rc::new(on_select),
        }
    }
}

impl RenderOnce for Segmented {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let mut group = div()
            .flex()
            .flex_none()
            .p(px(SPACE_1 / 2.0))
            .rounded(px(RADIUS_MD))
            .bg(hsla(p.bg_app));
        for (index, label) in self.options.into_iter().enumerate() {
            let id = ElementId::from((self.id.clone(), index));
            let handle = focus::handle(&id, window, cx);
            let chosen = index == self.selected;
            let on_select = self.on_select.clone();
            let hover = hsla(p.bg_hover);
            let mut option = div()
                .id(id)
                .track_focus(&handle)
                .tab_stop(true)
                .flex()
                .items_center()
                .h(px(24.))
                .px(px(SPACE_3 - 2.0))
                .rounded(px(RADIUS_SM))
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(if chosen { p.ink } else { p.ink_muted }))
                .cursor_pointer()
                .focus_visible(move |style| focus::ring(style, ring))
                .on_click(move |_, window, cx| on_select(index, window, cx))
                .child(label);
            option = if chosen {
                option.bg(hsla(p.bg_raised))
            } else {
                option.hover(move |style| style.bg(hover))
            };
            group = group.child(option);
        }
        group
    }
}
