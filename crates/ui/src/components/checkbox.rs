use gpui::{
    App, ClickEvent, ElementId, InteractiveElement, IntoElement, ParentElement, PathBuilder,
    RenderOnce, StatefulInteractiveElement, Styled, Window, canvas, div, point, px,
};
use tokens::{RADIUS_SM, SPACE_1};

use crate::convert::hsla;
use crate::focus;
use crate::theme::ThemeExt;

/// Box size: 16px, a multiple of the 4px grid.
const SIZE: f32 = 16.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Unchecked,
    Checked,
    /// Some but not all children are checked (a partly staged file).
    Mixed,
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// A checkbox: `line-control` border at rest, `accent` fill when checked or
/// mixed. The state lives in the caller; a click only reports.
#[derive(IntoElement)]
pub struct Checkbox {
    id: ElementId,
    state: CheckState,
    on_click: Option<ClickHandler>,
}

impl Checkbox {
    pub fn new(id: impl Into<ElementId>, state: CheckState) -> Self {
        Self {
            id: id.into(),
            state,
            on_click: None,
        }
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Checkbox {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let on = self.state != CheckState::Unchecked;
        let ring = focus::ring_color(cx);
        let handle = focus::handle(&self.id, window, cx);
        let (mark, state) = (hsla(p.on_accent), self.state);
        let mut boxed = div()
            .id(self.id)
            .track_focus(&handle)
            .tab_stop(true)
            .flex_none()
            .size(px(SIZE))
            .rounded(px(RADIUS_SM))
            .border_1()
            .border_color(hsla(if on { p.accent } else { p.line_control }))
            .bg(hsla(if on { p.accent } else { p.bg_panel }))
            .cursor_pointer()
            .focus_visible(move |style| focus::ring(style, ring))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let mut path = PathBuilder::stroke(px(1.5));
                        let (x, y) = (bounds.origin.x, bounds.origin.y);
                        let inset = px(SPACE_1);
                        match state {
                            CheckState::Unchecked => return,
                            CheckState::Checked => {
                                path.move_to(point(x + px(3.5), y + px(8.)));
                                path.line_to(point(x + px(6.5), y + px(11.)));
                                path.line_to(point(x + px(12.), y + px(5.)));
                            }
                            CheckState::Mixed => {
                                path.move_to(point(x + inset, y + px(SIZE / 2.0)));
                                path.line_to(point(x + px(SIZE) - inset, y + px(SIZE / 2.0)));
                            }
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, mark);
                        }
                    },
                )
                .size_full(),
            );
        if let Some(handler) = self.on_click {
            boxed = boxed.on_click(move |event, window, cx| handler(event, window, cx));
        }
        boxed
    }
}
