use gpui::{
    AnyElement, App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div, px,
};
use tokens::{RADIUS_MD, RADIUS_SM, SPACE_2, SPACE_3, TEXT_BODY};

use crate::convert::hsla;
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerKind {
    /// Informational notice: `accent` text on `accent-soft`.
    Info,
    /// Completed action: `add-ink` text on `add-bg`.
    Success,
    /// A paused operation or conflicts: `warning` text on `warning-soft`.
    Warning,
    /// Something that went wrong: `danger` text on `bg-raised`.
    Error,
}

/// A message bar with a crisp leading accent indicator and optional wrapping actions.
#[derive(IntoElement)]
pub struct Banner {
    kind: BannerKind,
    message: SharedString,
    actions: Vec<AnyElement>,
}

impl Banner {
    pub fn new(kind: BannerKind, message: impl Into<SharedString>) -> Self {
        Self {
            kind,
            message: message.into(),
            actions: Vec::new(),
        }
    }

    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.actions.push(action.into_any_element());
        self
    }
}

impl RenderOnce for Banner {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let (fill, ink) = match self.kind {
            BannerKind::Info => (p.accent_soft, p.accent),
            BannerKind::Success => (p.add_bg, p.add_ink),
            BannerKind::Warning => (p.warning_soft, p.warning),
            BannerKind::Error => (p.bg_raised, p.danger),
        };
        let actions = (!self.actions.is_empty()).then(|| {
            div()
                .flex()
                .flex_wrap()
                .flex_none()
                .items_center()
                .gap(px(SPACE_2))
                .children(self.actions)
        });
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(SPACE_3))
            .w_full()
            .px(px(SPACE_3))
            .py(px(SPACE_2))
            .rounded(px(RADIUS_MD))
            .bg(hsla(fill))
            .border_1()
            .border_color(hsla(p.line))
            .text_style(&TEXT_BODY, cx)
            .text_color(hsla(ink))
            .child(
                div()
                    .flex_none()
                    .w(px(focus::ACCENT_BAR_W))
                    .h(px(tokens::SPACE_4))
                    .rounded(px(RADIUS_SM))
                    .bg(hsla(ink)),
            )
            .child(div().flex_1().min_w(px(0.)).child(self.message))
            .children(actions)
    }
}
