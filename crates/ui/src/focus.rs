//! The focus ring: 2px in the `focus` colour, outside the control, so the
//! control does not change size when it takes focus.

use gpui::{
    App, BoxShadow, ElementId, FocusHandle, Hsla, StyleRefinement, Styled, Window, point, px,
};

use crate::convert::hsla;
use crate::theme::ThemeExt;

/// Ring width in logical pixels (docs/design/README.md, "Focus").
pub const RING_WIDTH: f32 = 2.0;

/// The focus handle of a control, kept across frames under `id`.
pub fn handle(id: &ElementId, window: &mut Window, cx: &mut App) -> FocusHandle {
    window
        .use_keyed_state((id.clone(), "focus"), cx, |_, cx| cx.focus_handle())
        .read(cx)
        .clone()
}

/// The style a control takes while it has keyboard focus.
pub fn ring(style: StyleRefinement, color: Hsla) -> StyleRefinement {
    style.shadow(vec![BoxShadow {
        color,
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(RING_WIDTH),
        inset: false,
    }])
}

/// The ring colour for the active theme.
pub fn ring_color(cx: &App) -> Hsla {
    hsla(cx.palette().focus)
}
