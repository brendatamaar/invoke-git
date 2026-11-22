//! A vertical scrollbar for a `uniform_list`: a track and a thumb that can
//! be dragged. GPUI has none at the pinned revision (the Phase 0 spike drew
//! its own; this is that code made reusable).
//!
//! Dragging jumps the list straight to the pointer, with no work at all
//! per move, so a top-to-bottom drag through a huge list stays smooth: the
//! rows it lands on are built from data that is already in memory.

use gpui::{
    App, Bounds, DispatchPhase, ElementId, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, RenderOnce, Styled,
    UniformListScrollHandle, Window, canvas, div, point, px,
};

use crate::convert::hsla;
use crate::theme::ThemeExt;

/// Track width in logical pixels.
pub const WIDTH: f32 = 12.0;
/// The thumb never gets shorter than this, so it stays easy to grab.
pub const MIN_THUMB: f32 = 24.0;

/// Where the thumb sits for one frame. All values are in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thumb {
    pub top: f32,
    pub height: f32,
    /// The room the thumb can move in: `track - height`.
    pub room: f32,
    /// How far the content can scroll: `content - track`.
    pub max_scroll: f32,
}

/// The thumb for a track of `track` px showing content `content` px tall,
/// scrolled down by `scroll_top` px. `None` when everything fits.
pub fn thumb(track: f32, content: f32, scroll_top: f32) -> Option<Thumb> {
    if track <= 0.0 || content <= track {
        return None;
    }
    let height = (track * track / content).max(MIN_THUMB).min(track);
    let max_scroll = content - track;
    let room = track - height;
    let scroll = scroll_top.clamp(0.0, max_scroll);
    Some(Thumb {
        top: scroll / max_scroll * room,
        height,
        room,
        max_scroll,
    })
}

/// How far down to scroll so the thumb's top is at `thumb_top` px (the
/// inverse of [`thumb`]); clamped to the scrollable range.
pub fn scroll_for_thumb_top(thumb: &Thumb, thumb_top: f32) -> f32 {
    if thumb.room <= 0.0 {
        return 0.0;
    }
    thumb_top.clamp(0.0, thumb.room) / thumb.room * thumb.max_scroll
}

/// Where the thumb is grabbed when a press lands on the track at `local`
/// px from its top: inside the thumb it keeps that offset, on bare track the
/// thumb jumps to be centred on the pointer.
pub fn grab_offset(thumb: &Thumb, local: f32) -> f32 {
    if (thumb.top..thumb.top + thumb.height).contains(&local) {
        local - thumb.top
    } else {
        thumb.height / 2.0
    }
}

#[derive(Default)]
struct State {
    track: Bounds<Pixels>,
    /// While dragging: where inside the thumb it was grabbed.
    drag: Option<f32>,
}

#[derive(IntoElement)]
pub struct Scrollbar {
    id: ElementId,
    handle: UniformListScrollHandle,
    items: usize,
    row_height: f32,
}

impl Scrollbar {
    /// For the `uniform_list` tracked by `handle`, with `items` rows of
    /// `row_height` px.
    pub fn new(
        id: impl Into<ElementId>,
        handle: UniformListScrollHandle,
        items: usize,
        row_height: f32,
    ) -> Self {
        Self {
            id: id.into(),
            handle,
            items,
            row_height,
        }
    }
}

fn scroll_top(handle: &UniformListScrollHandle) -> f32 {
    -f32::from(handle.0.borrow().base_handle.offset().y)
}

fn scroll_to(handle: &UniformListScrollHandle, top: f32) {
    handle
        .0
        .borrow()
        .base_handle
        .set_offset(point(px(0.), px(-top)));
}

impl RenderOnce for Scrollbar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let state =
            window.use_keyed_state((self.id.clone(), "scrollbar"), cx, |_, _| State::default());
        let track_height = f32::from(state.read(cx).track.size.height);
        let content = self.items as f32 * self.row_height;
        let current = thumb(track_height, content, scroll_top(&self.handle));
        let dragging = state.read(cx).drag.is_some();
        let thumb_color = if dragging {
            p.ink_faint
        } else {
            p.line_control
        };

        let measure = state.clone();
        let (down_state, down_handle) = (state.clone(), self.handle.clone());
        let (move_state, move_handle) = (state.clone(), self.handle.clone());
        let up_state = state.clone();
        let (items, row_height) = (self.items, self.row_height);

        div()
            .id(self.id)
            .flex_none()
            .w(px(WIDTH))
            .h_full()
            .relative()
            .bg(hsla(p.bg_panel))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        measure.update(cx, |state, _| state.track = bounds);
                    },
                    move |_, _, window, _| {
                        let (state, handle) = (move_state.clone(), move_handle.clone());
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase != DispatchPhase::Bubble {
                                return;
                            }
                            state.update(cx, |state, cx| {
                                let Some(grab) = state.drag else { return };
                                if !event.dragging() {
                                    // Released outside the window.
                                    state.drag = None;
                                    cx.notify();
                                    return;
                                }
                                let track = f32::from(state.track.size.height);
                                let Some(t) = thumb(track, items as f32 * row_height, 0.0) else {
                                    return;
                                };
                                let local = f32::from(event.position.y - state.track.origin.y);
                                scroll_to(&handle, scroll_for_thumb_top(&t, local - grab));
                                cx.notify();
                            });
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
            .children(current.map(|t| {
                div()
                    .absolute()
                    .top(px(t.top))
                    .left(px(2.))
                    .w(px(WIDTH - 4.))
                    .h(px(t.height))
                    .rounded(px(4.))
                    .bg(hsla(thumb_color))
            }))
            .on_mouse_down(MouseButton::Left, move |event: &MouseDownEvent, _, cx| {
                down_state.update(cx, |state, cx| {
                    let track = f32::from(state.track.size.height);
                    let Some(t) = thumb(track, items as f32 * row_height, scroll_top(&down_handle))
                    else {
                        return;
                    };
                    let local = f32::from(event.position.y - state.track.origin.y);
                    let grab = grab_offset(&t, local);
                    state.drag = Some(grab);
                    scroll_to(&down_handle, scroll_for_thumb_top(&t, local - grab));
                    cx.notify();
                });
            })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn nothing_to_scroll_has_no_thumb() {
        assert_eq!(thumb(500.0, 400.0, 0.0), None);
        assert_eq!(thumb(0.0, 400.0, 0.0), None);
    }

    #[test]
    fn the_thumb_is_proportional_and_never_tiny() {
        let t = thumb(500.0, 1000.0, 0.0).unwrap();
        assert_eq!(t.height, 250.0);
        let huge = thumb(500.0, 1_000_000.0, 0.0).unwrap();
        assert_eq!(huge.height, MIN_THUMB);
    }

    #[test]
    fn the_ends_meet_the_ends() {
        let t = thumb(500.0, 1000.0, 0.0).unwrap();
        assert_eq!(t.top, 0.0);
        let bottom = thumb(500.0, 1000.0, 500.0).unwrap();
        assert_eq!(bottom.top + bottom.height, 500.0);
    }

    proptest! {
        /// The thumb always lies inside the track, whatever the scroll.
        #[test]
        fn the_thumb_stays_in_the_track(
            track in 1.0f32..2000.0,
            extra in 1.0f32..1.0e7,
            scroll in -1.0e7f32..1.0e7,
        ) {
            let t = thumb(track, track + extra, scroll).unwrap();
            prop_assert!(t.top >= 0.0);
            prop_assert!(t.top + t.height <= track + 0.01);
            prop_assert!(t.height >= MIN_THUMB.min(track));
        }

        /// Dragging the thumb to where it already is gives back the same scroll.
        #[test]
        fn thumb_and_scroll_are_inverses(
            track in 100.0f32..2000.0,
            extra in 100.0f32..1.0e6,
            fraction in 0.0f32..=1.0,
        ) {
            let content = track + extra;
            let scroll = fraction * extra;
            let t = thumb(track, content, scroll).unwrap();
            let back = scroll_for_thumb_top(&t, t.top);
            prop_assert!((back - scroll).abs() <= extra * 1e-4 + 0.01, "{back} vs {scroll}");
        }

        /// The scroll a drag asks for is always inside the scrollable range.
        #[test]
        fn a_drag_never_scrolls_out_of_range(
            track in 100.0f32..2000.0,
            extra in 100.0f32..1.0e6,
            pointer in -5000.0f32..5000.0,
        ) {
            let t = thumb(track, track + extra, 0.0).unwrap();
            let scroll = scroll_for_thumb_top(&t, pointer);
            prop_assert!((0.0..=t.max_scroll + 0.01).contains(&scroll));
        }

        /// Pressing on bare track centres the thumb; pressing on the thumb keeps the grab.
        #[test]
        fn grab_keeps_or_centres(
            track in 100.0f32..2000.0,
            extra in 100.0f32..1.0e6,
            fraction in 0.0f32..=1.0,
            local in 0.0f32..2000.0,
        ) {
            let t = thumb(track, track + extra, fraction * extra).unwrap();
            let grab = grab_offset(&t, local);
            if (t.top..t.top + t.height).contains(&local) {
                prop_assert!((grab - (local - t.top)).abs() < 1e-3);
            } else {
                prop_assert!((grab - t.height / 2.0).abs() < 1e-3);
            }
        }
    }
}
