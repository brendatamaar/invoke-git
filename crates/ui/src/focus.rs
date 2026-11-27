//! The focus ring: 2px in the `focus` colour, outside the control by default
//! (or inset for fixed-height 20px rows so list viewports do not clip it).

use gpui::{
    App, BoxShadow, ElementId, FocusHandle, Hsla, StyleRefinement, Styled, Window, point, px,
};
use tokens::SPACE_1;

use crate::convert::hsla;
use crate::theme::ThemeExt;

/// Ring width in logical pixels (docs/design/README.md, "Focus"), half of `space-1`.
pub const RING_WIDTH: f32 = SPACE_1 / 2.0;

/// Width/height of the 2px accent selection bar used on active rows, tabs, and banners.
pub const ACCENT_BAR_W: f32 = SPACE_1 / 2.0;

/// The focus handle of a tab-focusable control, kept across frames under `id`.
pub fn handle(id: &ElementId, window: &mut Window, cx: &mut App) -> FocusHandle {
    handle_with_tab_stop(id, true, window, cx)
}

/// The focus handle of a control with explicit tab-stop participation.
///
/// Updates the handle itself on every frame because pinned GPUI reads
/// `tab_stop` from the supplied `FocusHandle` rather than from `.tab_stop()`
/// on a div that also calls `.track_focus()`.
pub fn handle_with_tab_stop(
    id: &ElementId,
    tab_stop: bool,
    window: &mut Window,
    cx: &mut App,
) -> FocusHandle {
    window
        .use_keyed_state((id.clone(), "focus"), cx, |_, cx| {
            cx.focus_handle().tab_stop(tab_stop)
        })
        .read(cx)
        .clone()
        .tab_stop(tab_stop)
}

/// The style a control takes while it has keyboard focus (2px outer ring).
pub fn ring(style: StyleRefinement, color: Hsla) -> StyleRefinement {
    style.shadow(vec![BoxShadow {
        color,
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(RING_WIDTH),
        inset: false,
    }])
}

/// An inset 2px focus ring for controls that fill a fixed-height `ROW_DIFF`
/// row, preventing overflow into adjacent rows or clipping at list edges.
pub fn inset_ring(style: StyleRefinement, color: Hsla) -> StyleRefinement {
    style.shadow(vec![BoxShadow {
        color,
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(RING_WIDTH),
        inset: true,
    }])
}

/// The ring colour for the active theme.
pub fn ring_color(cx: &App) -> Hsla {
    hsla(cx.palette().focus)
}
