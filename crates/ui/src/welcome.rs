//! The screen shown when no repo tab is active: open, init, and the errors
//! from trying.

use std::rc::Rc;

use gpui::{AnyElement, App, IntoElement, ParentElement, RenderOnce, Styled, Window, div, px};
use tokens::{SPACE_2, SPACE_4, SPACE_6, TEXT_BODY, TEXT_TITLE};

use crate::components::Button;
use crate::convert::hsla;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

type OnClick = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct Welcome {
    on_open: OnClick,
    on_init: OnClick,
    on_clone: Option<OnClick>,
    notices: Vec<AnyElement>,
}

impl Welcome {
    pub fn new(
        on_open: impl Fn(&mut Window, &mut App) + 'static,
        on_init: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            on_open: Rc::new(on_open),
            on_init: Rc::new(on_init),
            on_clone: None,
            notices: Vec::new(),
        }
    }

    /// Adds the "Clone repository" button.
    pub fn on_clone(mut self, on_clone: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_clone = Some(Rc::new(on_clone));
        self
    }

    /// An error or hint shown above the buttons (a banner).
    pub fn notice(mut self, notice: impl IntoElement) -> Self {
        self.notices.push(notice.into_any_element());
        self
    }
}

impl RenderOnce for Welcome {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let (on_open, on_init, on_clone) = (self.on_open, self.on_init, self.on_clone);
        div().flex().flex_1().items_center().justify_center().child(
            div()
                .flex()
                .flex_col()
                .gap(px(SPACE_4))
                .w(px(480.))
                .child(
                    div()
                        .text_style(&TEXT_TITLE, cx)
                        .text_color(hsla(p.ink))
                        .child("Open a repository"),
                )
                .child(
                    div()
                        .text_style(&TEXT_BODY, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(
                            "Pick a folder that holds a git repository, or start a new one in an empty folder.",
                        ),
                )
                .children(self.notices)
                .child(
                    div()
                        .flex()
                        .gap(px(SPACE_2))
                        .pt(px(SPACE_6 - SPACE_4))
                        .child(
                            Button::new("welcome-open", "Open repository…")
                                .primary()
                                .on_click(move |_, window, cx| on_open(window, cx)),
                        )
                        .child(
                            Button::new("welcome-init", "Initialize repository…")
                                .on_click(move |_, window, cx| on_init(window, cx)),
                        )
                        .children(on_clone.map(|on_clone| {
                            Button::new("welcome-clone", "Clone repository…")
                                .on_click(move |_, window, cx| on_clone(window, cx))
                        })),
                ),
        )
    }
}
