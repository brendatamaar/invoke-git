use gpui::{
    App, AppContext, ClickEvent, Context, CursorStyle, ElementId, FontWeight, InteractiveElement,
    IntoElement, ParentElement, Render, RenderOnce, Role, SharedString, StatefulInteractiveElement,
    Styled, Window, div, px,
};
use tokens::{
    RADIUS_MD, RADIUS_SM, ROW_DIFF, ROW_LIST, SPACE_1, SPACE_2, SPACE_3, TEXT_BODY_MEDIUM,
    TEXT_SMALL,
};

use crate::components::{Icon, IconName};
use crate::convert::hsla;
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// `accent` fill. At most one per view (Commit, Push).
    Primary,
    /// `bg-raised` fill with a hairline border, `bg-hover` on hover.
    Secondary,
    /// `danger` fill. Only after a description of what will change.
    Danger,
    /// No fill, `ink-muted` label, `bg-hover` + `ink` on hover.
    Ghost,
    /// Quiet destructive trigger: `ink-muted` at rest, `danger` text + `bg-hover` on hover.
    DangerGhost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonSize {
    /// Standard 28px (`ROW_LIST`) button.
    #[default]
    Default,
    /// Dense 20px (`ROW_DIFF`) button for inline toolbars, headers and rows.
    Compact,
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// A button with a text label (28px by default, 20px when `.compact()`).
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    icon: Option<IconName>,
    shortcut: Option<SharedString>,
    tooltip: Option<SharedString>,
    kind: ButtonKind,
    size: ButtonSize,
    full_width: bool,
    disabled: bool,
    on_click: Option<ClickHandler>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            shortcut: None,
            tooltip: None,
            kind: ButtonKind::Secondary,
            size: ButtonSize::Default,
            full_width: false,
            disabled: false,
            on_click: None,
        }
    }

    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Optional keyboard shortcut hint shown quietly beside the label.
    pub fn shortcut(mut self, shortcut: impl Into<SharedString>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    /// Explains an icon-only action or provides detail about a visible label.
    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
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

    pub fn danger_ghost(self) -> Self {
        self.kind(ButtonKind::DangerGhost)
    }

    pub fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }

    /// 20px tall (`ROW_DIFF`), tighter padding, small medium label.
    pub fn compact(self) -> Self {
        self.size(ButtonSize::Compact)
    }

    /// Expands to fill the parent's width (e.g. Commit action or menu row).
    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
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

pub(crate) fn text_tooltip(text: SharedString, cx: &mut App) -> gpui::AnyView {
    cx.new(|_| ButtonTooltip(text)).into()
}

struct ButtonTooltip(SharedString);

impl Render for ButtonTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        div()
            .px(px(SPACE_2))
            .py(px(SPACE_1))
            .rounded(px(RADIUS_SM))
            .border_1()
            .border_color(hsla(p.line))
            .bg(hsla(p.bg_raised))
            .text_style(&TEXT_SMALL, cx)
            .text_color(hsla(p.ink))
            .child(self.0.clone())
    }
}

impl RenderOnce for Button {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let handle = focus::handle_with_tab_stop(&self.id, !self.disabled, window, cx);
        let (height, pad_x, radius) = match self.size {
            ButtonSize::Default => (ROW_LIST, SPACE_3, RADIUS_MD),
            ButtonSize::Compact => (ROW_DIFF, SPACE_2, RADIUS_SM),
        };
        let mut button = div()
            .id(self.id)
            .role(Role::Button)
            .aria_label(if self.label.is_empty() {
                self.tooltip.clone().unwrap_or_default()
            } else {
                self.label.clone()
            })
            .track_focus(&handle)
            .tab_stop(!self.disabled)
            .flex()
            .items_center()
            .justify_center()
            .gap(px(SPACE_1))
            .whitespace_nowrap()
            .h(px(height))
            .px(px(pad_x))
            .rounded(px(radius));
        button = if self.full_width {
            button.w_full()
        } else {
            button.flex_none()
        };
        button = match self.size {
            ButtonSize::Default => button.text_style(&TEXT_BODY_MEDIUM, cx),
            ButtonSize::Compact => button
                .text_style(&TEXT_SMALL, cx)
                .font_weight(FontWeight(500.0)),
        };
        let icon_color = match self.kind {
            ButtonKind::Primary => p.on_accent,
            ButtonKind::Secondary => p.ink_muted,
            ButtonKind::Danger => p.on_danger,
            ButtonKind::Ghost | ButtonKind::DangerGhost => p.ink_muted,
        };
        button = match self.kind {
            ButtonKind::Primary => button.bg(hsla(p.accent)).text_color(hsla(p.on_accent)),
            ButtonKind::Secondary => button
                .bg(hsla(p.bg_raised))
                .border_1()
                .border_color(hsla(p.line))
                .text_color(hsla(p.ink)),
            ButtonKind::Danger => button.bg(hsla(p.danger)).text_color(hsla(p.on_danger)),
            ButtonKind::Ghost | ButtonKind::DangerGhost => button.text_color(hsla(p.ink_muted)),
        };
        if let Some(icon) = self.icon {
            let icon_el = if self.size == ButtonSize::Compact {
                Icon::new(icon).small().color(icon_color)
            } else {
                Icon::new(icon).color(icon_color)
            };
            button = button.child(icon_el);
        }
        if !self.label.is_empty() {
            if let Some(description) = self.tooltip.as_ref().filter(|text| **text != self.label) {
                button = button.aria_description(description.clone());
            }
            button = button.child(self.label);
        }
        if let Some(shortcut) = self.shortcut {
            button = button.child(
                div()
                    .flex_none()
                    .pl(px(SPACE_1))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(match self.kind {
                        ButtonKind::Primary => p.on_accent,
                        ButtonKind::Danger => p.on_danger,
                        _ => p.ink_faint,
                    }))
                    .child(shortcut),
            );
        }
        if let Some(text) = self.tooltip {
            button = button.tooltip(move |_, cx| text_tooltip(text.clone(), cx));
        }
        if self.disabled {
            return button.opacity(0.4).cursor(CursorStyle::OperationNotAllowed);
        }
        let hover_bg = hsla(p.bg_hover);
        let hover_border = hsla(p.line_control);
        let hover_ink = hsla(p.ink);
        let hover_danger = hsla(p.danger);
        let compact = self.size == ButtonSize::Compact;
        button = button.cursor_pointer().focus_visible(move |style| {
            if compact {
                focus::inset_ring(style, ring)
            } else {
                focus::ring(style, ring)
            }
        });
        button = match self.kind {
            ButtonKind::Primary | ButtonKind::Danger => button
                .hover(|style| style.opacity(0.9))
                .active(|style| style.opacity(0.8)),
            ButtonKind::Secondary => button
                .hover(move |style| style.bg(hover_bg).border_color(hover_border))
                .active(|style| style.opacity(0.85)),
            ButtonKind::Ghost => button
                .hover(move |style| style.bg(hover_bg).text_color(hover_ink))
                .active(|style| style.opacity(0.8)),
            ButtonKind::DangerGhost => button
                .hover(move |style| style.bg(hover_bg).text_color(hover_danger))
                .active(|style| style.opacity(0.8)),
        };
        match self.on_click {
            Some(handler) => button.on_click(move |event, window, cx| handler(event, window, cx)),
            None => button,
        }
    }
}
