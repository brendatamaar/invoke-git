use gpui::{
    App, ClickEvent, CursorStyle, ElementId, InteractiveElement, IntoElement, ParentElement,
    RenderOnce, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use tokens::{RADIUS_MD, ROW_LIST, SPACE_3, TEXT_BODY_MEDIUM};

use crate::convert::hsla;
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// `accent` fill. At most one per view (Commit, Push).
    Primary,
    /// `bg-raised` fill, `bg-hover` on hover.
    Secondary,
    /// `danger` fill. Only after a description of what will change.
    Danger,
    /// No fill, `ink-muted` label, `bg-hover` on hover.
    Ghost,
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// A 28px button with a text label.
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    kind: ButtonKind,
    disabled: bool,
    on_click: Option<ClickHandler>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: ButtonKind::Secondary,
            disabled: false,
            on_click: None,
        }
    }

    pub fn kind(mut self, kind: ButtonKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn primary(self) -> Self {
        self.kind(ButtonKind::Primary)
    }

    pub fn danger(self) -> Self {
        self.kind(ButtonKind::Danger)
    }

    pub fn ghost(self) -> Self {
        self.kind(ButtonKind::Ghost)
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let (fill, fg, hover) = match self.kind {
            ButtonKind::Primary => (Some(p.accent), p.on_accent, None),
            ButtonKind::Secondary => (Some(p.bg_raised), p.ink, Some(p.bg_hover)),
            ButtonKind::Danger => (Some(p.danger), p.on_danger, None),
            ButtonKind::Ghost => (None, p.ink_muted, Some(p.bg_hover)),
        };
        let ring = focus::ring_color(cx);
        let handle = focus::handle(&self.id, window, cx);
        let mut button = div()
            .id(self.id)
            .track_focus(&handle)
            .tab_stop(!self.disabled)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(ROW_LIST))
            .px(px(SPACE_3))
            .rounded(px(RADIUS_MD))
            .text_style(&TEXT_BODY_MEDIUM, cx)
            .text_color(hsla(fg))
            .child(self.label);
        if let Some(fill) = fill {
            button = button.bg(hsla(fill));
        }
        if self.disabled {
            return button.opacity(0.4).cursor(CursorStyle::OperationNotAllowed);
        }
        button = button
            .cursor_pointer()
            .focus_visible(move |style| focus::ring(style, ring));
        if let Some(hover) = hover {
            button = button.hover(move |style| style.bg(hsla(hover)));
        }
        match self.on_click {
            Some(handler) => button.on_click(move |event, window, cx| handler(event, window, cx)),
            None => button,
        }
    }
}
