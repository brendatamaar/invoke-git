//! A vertical drag handle for resizing split panels.
//!
//! Pure width math (`clamp_width`, `drag_width`, `nudge_width`) is tested
//! without a window; [`ResizeHandle`] renders a token-sized grab target with a
//! crisp 1px divider that highlights on hover, drag, or keyboard focus.
//! Dragging tracks window-level pointer moves (so fast drags never stall),
//! double-clicking resets to the default width, and Left/Right/Enter resize or
//! reset when focused by keyboard.

use std::rc::Rc;

use gpui::{
    App, DispatchPhase, ElementId, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, RenderOnce, Styled, Window,
    canvas, div, px,
};
use tokens::{SPACE_1, SPACE_4};

use crate::convert::hsla;
use crate::focus;
use crate::theme::ThemeExt;

/// Interactive hit-target width on the 4px token grid.
pub const HANDLE_WIDTH: f32 = SPACE_1;

/// Keyboard resize step when pressing Left / Right on a focused handle.
pub const KEY_STEP: f32 = SPACE_4;

/// Which edge of the resized panel the handle sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeEdge {
    /// Handle sits on the right edge of a left-docked panel (`+dx` widens).
    Right,
    /// Handle sits on the left edge of a right-docked panel (`-dx` widens).
    Left,
    /// Handle sits on the bottom edge of a top-docked panel (`+dy` grows taller).
    Bottom,
    /// Handle sits on the top edge of a bottom-docked panel (`-dy` grows taller).
    Top,
}

impl ResizeEdge {
    pub const fn is_vertical(self) -> bool {
        matches!(self, ResizeEdge::Top | ResizeEdge::Bottom)
    }
}

/// Clamp `width` into `[min, max]`, handling reversed bounds and non-finite inputs.
pub fn clamp_width(width: f32, min: f32, max: f32) -> f32 {
    let (lo, hi) = if min <= max { (min, max) } else { (max, min) };
    if !width.is_finite() {
        return lo;
    }
    width.clamp(lo, hi)
}

/// Compute the updated panel width for a pointer drag from `start_x` to
/// `current_x`, rounded to whole logical pixels and clamped to `[min, max]`.
pub fn drag_width(
    start_width: f32,
    start_x: f32,
    current_x: f32,
    edge: ResizeEdge,
    min: f32,
    max: f32,
) -> f32 {
    if !start_width.is_finite() || !start_x.is_finite() || !current_x.is_finite() {
        return clamp_width(start_width, min, max);
    }
    let delta = match edge {
        ResizeEdge::Right | ResizeEdge::Bottom => current_x - start_x,
        ResizeEdge::Left | ResizeEdge::Top => start_x - current_x,
    };
    clamp_width((start_width + delta).round(), min, max)
}

/// Compute the updated panel width for a keyboard nudge (`direction`: `-1` for
/// Left/Up arrow, `+1` for Right/Down arrow).
pub fn nudge_width(
    current: f32,
    direction: i8,
    step: f32,
    edge: ResizeEdge,
    min: f32,
    max: f32,
) -> f32 {
    let signed = f32::from(direction.signum()) * step.abs();
    let delta = match edge {
        ResizeEdge::Right | ResizeEdge::Bottom => signed,
        ResizeEdge::Left | ResizeEdge::Top => -signed,
    };
    clamp_width((current + delta).round(), min, max)
}

#[derive(Clone, Copy)]
struct Drag {
    start_width: f32,
    start_x: f32,
    last_width: f32,
}

#[derive(Default)]
struct State {
    drag: Option<Drag>,
}

type ResizeHandler = Rc<dyn Fn(f32, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct ResizeHandle {
    id: ElementId,
    current: f32,
    default: f32,
    min: f32,
    max: f32,
    edge: ResizeEdge,
    on_resize: ResizeHandler,
}

impl ResizeHandle {
    pub fn new(
        id: impl Into<ElementId>,
        current: f32,
        default: f32,
        min: f32,
        max: f32,
        edge: ResizeEdge,
        on_resize: impl Fn(f32, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            current,
            default,
            min,
            max,
            edge,
            on_resize: Rc::new(on_resize),
        }
    }
}

impl RenderOnce for ResizeHandle {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let state =
            window.use_keyed_state((self.id.clone(), "resize"), cx, |_, _| State::default());
        let focus_handle = focus::handle(&self.id, window, cx);
        let dragging = state.read(cx).drag.is_some();

        let (current, default, min, max, edge) =
            (self.current, self.default, self.min, self.max, self.edge);

        let move_state = state.clone();
        let move_cb = self.on_resize.clone();
        let up_state = state.clone();
        let down_state = state.clone();
        let down_cb = self.on_resize.clone();
        let key_cb = self.on_resize;

        let line_color = if dragging { p.accent } else { p.line };
        let line_width = if dragging { focus::ACCENT_BAR_W } else { 1.0 };
        let accent = hsla(p.accent);

        let vertical = edge.is_vertical();
        let bar = if vertical {
            div()
                .h(px(line_width))
                .w_full()
                .bg(hsla(line_color))
                .group_hover("resize-handle", move |style| {
                    style.h(px(focus::ACCENT_BAR_W)).bg(accent)
                })
        } else {
            div()
                .w(px(line_width))
                .h_full()
                .bg(hsla(line_color))
                .group_hover("resize-handle", move |style| {
                    style.w(px(focus::ACCENT_BAR_W)).bg(accent)
                })
        };

        let base = div()
            .id(self.id)
            .track_focus(&focus_handle)
            .tab_stop(true)
            .group("resize-handle")
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .bg(hsla(p.bg_panel))
            .focus_visible(move |style| focus::ring(style, ring));
        let base = if vertical {
            base.h(px(HANDLE_WIDTH)).w_full().cursor_row_resize()
        } else {
            base.w(px(HANDLE_WIDTH)).h_full().cursor_col_resize()
        };

        base.child(bar)
            .child(
                canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        let (state, cb) = (move_state.clone(), move_cb.clone());
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                            if phase != DispatchPhase::Bubble {
                                return;
                            }
                            let Some(drag) = state.read(cx).drag else {
                                return;
                            };
                            if !event.dragging() {
                                state.update(cx, |state, cx| {
                                    state.drag = None;
                                    cx.notify();
                                });
                                return;
                            }
                            let pos = if vertical {
                                f32::from(event.position.y)
                            } else {
                                f32::from(event.position.x)
                            };
                            let next =
                                drag_width(drag.start_width, drag.start_x, pos, edge, min, max);
                            if (next - drag.last_width).abs() >= 0.5 {
                                state.update(cx, |state, _| {
                                    if let Some(active) = &mut state.drag {
                                        active.last_width = next;
                                    }
                                });
                                cb(next, window, cx);
                            }
                        });
                        let state = up_state.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                                state.update(cx, |state, cx| {
                                    if state.drag.take().is_some() {
                                        cx.notify();
                                    }
                                });
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
            .on_mouse_down(
                MouseButton::Left,
                move |event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if event.click_count >= 2 {
                        down_state.update(cx, |state, cx| {
                            state.drag = None;
                            cx.notify();
                        });
                        down_cb(clamp_width(default, min, max), window, cx);
                        return;
                    }
                    let start_width = clamp_width(current, min, max);
                    let start_pos = if vertical {
                        f32::from(event.position.y)
                    } else {
                        f32::from(event.position.x)
                    };
                    down_state.update(cx, |state, cx| {
                        state.drag = Some(Drag {
                            start_width,
                            start_x: start_pos,
                            last_width: start_width,
                        });
                        cx.notify();
                    });
                },
            )
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                let m = event.keystroke.modifiers;
                if m.control || m.alt || m.platform || m.function {
                    return;
                }
                let next = match (vertical, event.keystroke.key.as_str()) {
                    (false, "left") | (true, "up") => {
                        nudge_width(current, -1, KEY_STEP, edge, min, max)
                    }
                    (false, "right") | (true, "down") => {
                        nudge_width(current, 1, KEY_STEP, edge, min, max)
                    }
                    (_, "enter" | "home" | "0") => clamp_width(default, min, max),
                    _ => return,
                };
                cx.stop_propagation();
                if (next - current).abs() >= 0.5 {
                    key_cb(next, window, cx);
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn clamp_handles_bounds_and_non_finite() {
        assert_eq!(clamp_width(240.0, 200.0, 360.0), 240.0);
        assert_eq!(clamp_width(100.0, 200.0, 360.0), 200.0);
        assert_eq!(clamp_width(500.0, 200.0, 360.0), 360.0);
        assert_eq!(clamp_width(f32::NAN, 200.0, 360.0), 200.0);
        assert_eq!(clamp_width(f32::INFINITY, 200.0, 360.0), 200.0);
        assert_eq!(clamp_width(250.0, 360.0, 200.0), 250.0);
    }

    #[test]
    fn right_and_left_edges_move_in_opposite_directions() {
        assert_eq!(
            drag_width(240.0, 240.0, 280.0, ResizeEdge::Right, 200.0, 360.0),
            280.0
        );
        assert_eq!(
            drag_width(520.0, 700.0, 650.0, ResizeEdge::Left, 320.0, 820.0),
            570.0
        );
    }

    #[test]
    fn keyboard_nudge_respects_edge_and_bounds() {
        assert_eq!(
            nudge_width(240.0, 1, KEY_STEP, ResizeEdge::Right, 200.0, 360.0),
            240.0 + KEY_STEP
        );
        assert_eq!(
            nudge_width(240.0, -1, KEY_STEP, ResizeEdge::Right, 200.0, 360.0),
            240.0 - KEY_STEP
        );
        assert_eq!(
            nudge_width(520.0, -1, KEY_STEP, ResizeEdge::Left, 320.0, 820.0),
            520.0 + KEY_STEP
        );
        assert_eq!(
            nudge_width(520.0, 1, KEY_STEP, ResizeEdge::Left, 320.0, 820.0),
            520.0 - KEY_STEP
        );
    }

    proptest! {
        /// Drag width always stays within `[min, max]` for any pointer move.
        #[test]
        fn drag_width_is_always_in_bounds(
            min in 100.0f32..400.0,
            span in 0.0f32..600.0,
            start_width in 0.0f32..1200.0,
            start_x in -2000.0f32..2000.0,
            current_x in -2000.0f32..2000.0,
            left in any::<bool>(),
        ) {
            let max = min + span;
            let edge = if left { ResizeEdge::Left } else { ResizeEdge::Right };
            let width = drag_width(start_width, start_x, current_x, edge, min, max);
            prop_assert!((min..=max).contains(&width), "{width} not in [{min}, {max}]");
        }

        /// Moving further right monotonically widens a right-edge panel and narrows a left-edge panel.
        #[test]
        fn drag_width_is_monotonic(
            min in 100.0f32..300.0,
            span in 50.0f32..500.0,
            start_width in 100.0f32..800.0,
            start_x in -1000.0f32..1000.0,
            x1 in -1000.0f32..1000.0,
            dx in 0.0f32..1000.0,
        ) {
            let max = min + span;
            let x2 = x1 + dx;
            let r1 = drag_width(start_width, start_x, x1, ResizeEdge::Right, min, max);
            let r2 = drag_width(start_width, start_x, x2, ResizeEdge::Right, min, max);
            prop_assert!(r2 >= r1, "right edge: {r2} < {r1}");

            let l1 = drag_width(start_width, start_x, x1, ResizeEdge::Left, min, max);
            let l2 = drag_width(start_width, start_x, x2, ResizeEdge::Left, min, max);
            prop_assert!(l2 <= l1, "left edge: {l2} > {l1}");
        }

        /// Keyboard nudging always stays inside `[min, max]`.
        #[test]
        fn nudge_width_stays_in_bounds(
            min in 100.0f32..400.0,
            span in 0.0f32..600.0,
            current in 0.0f32..1200.0,
            dir in -2i8..=2i8,
            step in 1.0f32..128.0,
            left in any::<bool>(),
        ) {
            let max = min + span;
            let edge = if left { ResizeEdge::Left } else { ResizeEdge::Right };
            let next = nudge_width(current, dir, step, edge, min, max);
            prop_assert!((min..=max).contains(&next), "{next} not in [{min}, {max}]");
        }
    }
}
