//! Token text styles applied to GPUI elements.

use gpui::{App, FontFeatures, FontWeight, Styled, px};
use tokens::{FontFamily, TextStyle};

use crate::theme::ThemeExt;

pub trait TextStyleExt: Styled + Sized {
    /// Family, size, line height and weight of a token text style. Letter
    /// spacing is not applied: GPUI's text style has no tracking. Code
    /// styles turn font ligatures off: a diff must show `->` and `!=` as
    /// typed, not as one joined glyph.
    fn text_style(self, style: &TextStyle, cx: &App) -> Self {
        let family = cx.ui_theme().family(style.family);
        let styled = self
            .font_family(family)
            .text_size(px(style.font_size))
            .line_height(px(style.line_height))
            .font_weight(FontWeight(f32::from(style.font_weight)));
        match style.family {
            FontFamily::Mono => styled.font_features(FontFeatures::disable_ligatures()),
            FontFamily::Sans => styled,
        }
    }
}

impl<T: Styled + Sized> TextStyleExt for T {}
