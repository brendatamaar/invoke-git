use gpui::{App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div};
use tokens::TEXT_LABEL;

use crate::convert::hsla;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// An uppercase section header in the `label` style and `ink-muted`.
#[derive(IntoElement)]
pub struct SectionLabel {
    text: SharedString,
}

impl SectionLabel {
    /// `text` is shown in uppercase whatever its case.
    pub fn new(text: impl AsRef<str>) -> Self {
        Self {
            text: SharedString::from(text.as_ref().to_uppercase()),
        }
    }
}

impl RenderOnce for SectionLabel {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .text_style(&TEXT_LABEL, cx)
            .text_color(hsla(cx.palette().ink_muted))
            .child(self.text)
    }
}
