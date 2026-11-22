use gpui::{App, IntoElement, RenderOnce, Styled, Window, div, px};
use tokens::SPACE_4;

use crate::convert::hsla;
use crate::theme::ThemeExt;

/// A 1px `line` divider. Horizontal fills the width; vertical is 20px tall,
/// as between toolbar groups.
#[derive(IntoElement)]
pub struct Divider {
    vertical: bool,
}

impl Divider {
    pub fn horizontal() -> Self {
        Self { vertical: false }
    }

    pub fn vertical() -> Self {
        Self { vertical: true }
    }
}

impl RenderOnce for Divider {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let line = div().flex_none().bg(hsla(cx.palette().line));
        if self.vertical {
            line.w(px(1.)).h(px(SPACE_4 + 4.0))
        } else {
            line.h(px(1.)).w_full()
        }
    }
}
