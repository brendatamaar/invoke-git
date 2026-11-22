use gpui::{App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div, px};
use tokens::{RADIUS_SM, SPACE_2, SPACE_6, TEXT_SMALL};

use crate::convert::hsla;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// The current-branch pill: `accent-soft` background, `accent` text.
#[derive(IntoElement)]
pub struct Pill {
    label: SharedString,
}

impl Pill {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

impl RenderOnce for Pill {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(SPACE_6))
            .px(px(SPACE_2))
            .rounded(px(RADIUS_SM))
            .bg(hsla(p.accent_soft))
            .text_color(hsla(p.accent))
            .text_style(&TEXT_SMALL, cx)
            .child(self.label)
    }
}
