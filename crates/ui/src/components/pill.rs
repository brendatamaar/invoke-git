use gpui::{
    App, FontWeight, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div, px,
};
use tokens::{RADIUS_SM, ROW_DIFF, SPACE_2, SPACE_6, TEXT_SMALL};

use crate::convert::hsla;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PillTone {
    /// Current branch / active state (`accent-soft` / `accent`).
    #[default]
    Accent,
    /// Neutral badge (`bg-raised` / `ink-muted` with hairline `line` border).
    Neutral,
    /// Open / approved / clean (`add-bg` / `add-ink`).
    Success,
    /// Warning / changes requested / conflict (`warning-soft` / `warning`).
    Warning,
    /// Closed / removed / error (`remove-bg` / `remove-ink`).
    Danger,
}

/// A compact status or ref pill.
#[derive(IntoElement)]
pub struct Pill {
    label: SharedString,
    tone: PillTone,
    compact: bool,
    max_width: Option<f32>,
}

impl Pill {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            tone: PillTone::Accent,
            compact: false,
            max_width: None,
        }
    }

    pub fn tone(mut self, tone: PillTone) -> Self {
        self.tone = tone;
        self
    }

    /// 20px tall (`ROW_DIFF`), for use inside 28px list rows.
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    /// Caps the pill width in logical pixels and truncates long labels with an ellipsis.
    pub fn max_w(mut self, width: f32) -> Self {
        self.max_width = Some(width);
        self
    }
}

impl RenderOnce for Pill {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let (fill, fg, bordered) = match self.tone {
            PillTone::Accent => (p.accent_soft, p.accent, false),
            PillTone::Neutral => (p.bg_raised, p.ink_muted, true),
            PillTone::Success => (p.add_bg, p.add_ink, false),
            PillTone::Warning => (p.warning_soft, p.warning, false),
            PillTone::Danger => (p.remove_bg, p.remove_ink, false),
        };
        let mut pill = div()
            .flex()
            .items_center()
            .h(px(if self.compact { ROW_DIFF } else { SPACE_6 }))
            .px(px(SPACE_2))
            .rounded(px(RADIUS_SM))
            .bg(hsla(fill))
            .text_color(hsla(fg))
            .text_style(&TEXT_SMALL, cx)
            .font_weight(FontWeight(500.0));
        if let Some(width) = self.max_width {
            pill = pill.min_w(px(0.)).max_w(px(width)).overflow_hidden();
        } else {
            pill = pill.flex_none().whitespace_nowrap();
        }
        if bordered {
            pill = pill.border_1().border_color(hsla(p.line));
        }
        if self.max_width.is_some() {
            pill.child(div().min_w(px(0.)).truncate().child(self.label))
        } else {
            pill.child(self.label)
        }
    }
}
