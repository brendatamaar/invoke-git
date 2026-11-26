use gpui::{
    App, ClickEvent, ElementId, InteractiveElement, IntoElement, ParentElement, PathBuilder,
    RenderOnce, SharedString, StatefulInteractiveElement, Styled, Window, canvas, div, point, px,
};
use tokens::{RADIUS_SM, SPACE_1, SPACE_2, SPACE_4, TEXT_SMALL};

use crate::convert::hsla;
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Unchecked,
    Checked,
    /// Some but not all children are checked (a partly staged file).
    Mixed,
}

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// A checkbox: `line-control` border at rest, `accent` fill when checked or
/// mixed. The state lives in the caller; a click only reports. When `.label(...)`
/// is set, the label shares the click target and hover state.
#[derive(IntoElement)]
pub struct Checkbox {
    id: ElementId,
    state: CheckState,
    label: Option<SharedString>,
    on_click: Option<ClickHandler>,
}

impl Checkbox {
    pub fn new(id: impl Into<ElementId>, state: CheckState) -> Self {
        Self {
            id: id.into(),
            state,
            label: None,
            on_click: None,
        }
    }

    /// Shows a clickable label next to the box.
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
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

impl RenderOnce for Checkbox {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let on = self.state != CheckState::Unchecked;
        let ring = focus::ring_color(cx);
        let handle = focus::handle(&self.id, window, cx);
        let (mark, state) = (hsla(p.on_accent), self.state);
        let accent = hsla(p.accent);
        let hover_bg = hsla(p.bg_hover);
        let ink = hsla(p.ink);

        let indicator = div()
            .flex_none()
            .size(px(SPACE_4))
            .rounded(px(RADIUS_SM))
            .border_1()
            .border_color(hsla(if on { p.accent } else { p.line_control }))
            .bg(hsla(if on { p.accent } else { p.bg_panel }))
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
                                path.move_to(point(x + inset, y + px(SPACE_4 / 2.0)));
                                path.line_to(point(x + px(SPACE_4) - inset, y + px(SPACE_4 / 2.0)));
                            }
                        }
                        if let Ok(path) = path.build() {
                            window.paint_path(path, mark);
                        }
                    },
                )
                .size_full(),
            );

        if let Some(label) = self.label {
            let indicator = if on {
                indicator.group_hover("checkbox", |style| style.opacity(0.9))
            } else {
                indicator.group_hover("checkbox", move |style| {
                    style.bg(hover_bg).border_color(accent)
                })
            };
            let mut row = div()
                .id(self.id)
                .group("checkbox")
                .track_focus(&handle)
                .tab_stop(true)
                .flex()
                .items_center()
                .gap(px(SPACE_2))
                .rounded(px(RADIUS_SM))
                .cursor_pointer()
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink_muted))
                .hover(move |style| style.text_color(ink))
                .focus_visible(move |style| focus::ring(style, ring))
                .child(indicator)
                .child(div().min_w(px(0.)).truncate().child(label));
            if let Some(handler) = self.on_click {
                row = row.on_click(move |event, window, cx| handler(event, window, cx));
            }
            return row;
        }

        let mut boxed = indicator
            .id(self.id)
            .track_focus(&handle)
            .tab_stop(true)
            .cursor_pointer()
            .focus_visible(move |style| focus::ring(style, ring));
        boxed = if on {
            boxed.hover(|style| style.opacity(0.9))
        } else {
            boxed.hover(move |style| style.bg(hover_bg).border_color(accent))
        };
        if let Some(handler) = self.on_click {
            boxed = boxed.on_click(move |event, window, cx| handler(event, window, cx));
        }
        boxed
    }
}
