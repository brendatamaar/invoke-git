//! The screen shown when no repo tab is active: open, init, clone, recent
//! repositories, and the errors from trying.

use std::rc::Rc;

use gpui::{
    AnyElement, App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use tokens::{
    DIALOG, NAME_COLUMN, RADIUS_LG, RADIUS_MD, RADIUS_SM, ROW_DIFF, ROW_LIST, SPACE_1, SPACE_2,
    SPACE_4, SPACE_6, SPACE_8, TEXT_BODY, TEXT_BODY_MEDIUM, TEXT_CODE_SMALL, TEXT_DISPLAY,
};

use crate::components::{Button, Divider, SectionLabel};
use crate::convert::{hsla, shadows};
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

type OnClick = Rc<dyn Fn(&mut Window, &mut App)>;
type OnIndex = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// One recently opened or closed repository shown on the welcome screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentRepo {
    pub title: SharedString,
    pub path: SharedString,
}

#[derive(IntoElement)]
pub struct Welcome {
    on_open: OnClick,
    on_init: OnClick,
    on_clone: Option<OnClick>,
    recent: Vec<RecentRepo>,
    on_open_recent: Option<OnIndex>,
    on_remove_recent: Option<OnIndex>,
    on_clear_recent: Option<OnClick>,
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
            recent: Vec::new(),
            on_open_recent: None,
            on_remove_recent: None,
            on_clear_recent: None,
            notices: Vec::new(),
        }
    }

    /// Adds the "Clone repository" button.
    pub fn on_clone(mut self, on_clone: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_clone = Some(Rc::new(on_clone));
        self
    }

    /// Lists recently opened or closed repositories for one-click reopening.
    pub fn recent(
        mut self,
        items: Vec<RecentRepo>,
        on_open: impl Fn(usize, &mut Window, &mut App) + 'static,
        on_remove: impl Fn(usize, &mut Window, &mut App) + 'static,
        on_clear: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        self.recent = items;
        self.on_open_recent = Some(Rc::new(on_open));
        self.on_remove_recent = Some(Rc::new(on_remove));
        self.on_clear_recent = Some(Rc::new(on_clear));
        self
    }

    /// An error or hint shown above the buttons (a banner).
    pub fn notice(mut self, notice: impl IntoElement) -> Self {
        self.notices.push(notice.into_any_element());
        self
    }
}

impl RenderOnce for Welcome {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let (on_open, on_init, on_clone) = (self.on_open, self.on_init, self.on_clone);

        let recent_section = (!self.recent.is_empty()).then(|| {
            let hover_bg = hsla(p.bg_hover);
            let remove_hover_bg = hsla(p.bg_raised);
            let hover_ink = hsla(p.ink);
            let on_open_recent = self.on_open_recent;
            let on_remove_recent = self.on_remove_recent;
            let on_clear_recent = self.on_clear_recent;

            let mut list = div()
                .id("welcome-recent-list")
                .flex()
                .flex_col()
                .gap(px(SPACE_1))
                .max_h(px(ROW_LIST * 6.0 + SPACE_1 * 5.0))
                .overflow_y_scroll();

            for (index, item) in self.recent.into_iter().enumerate() {
                let row_id = ElementId::from(("welcome-recent", index));
                let row_handle = focus::handle(&row_id, window, cx);
                let remove_id = ElementId::from(("welcome-recent-remove", index));
                let remove_handle = focus::handle(&remove_id, window, cx);
                let open_cb = on_open_recent.clone();
                let remove_cb = on_remove_recent.clone();

                let mut row = div()
                    .id(row_id)
                    .track_focus(&row_handle)
                    .tab_stop(true)
                    .flex()
                    .items_center()
                    .gap(px(SPACE_2))
                    .h(px(ROW_LIST))
                    .px(px(SPACE_2))
                    .rounded(px(RADIUS_MD))
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover_bg))
                    .focus_visible(move |style| focus::inset_ring(style, ring))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(NAME_COLUMN * 0.6))
                            .truncate()
                            .text_style(&TEXT_BODY_MEDIUM, cx)
                            .text_color(hsla(p.ink))
                            .child(item.title),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_style(&TEXT_CODE_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(item.path),
                    );
                if let Some(open_cb) = open_cb {
                    row = row.on_click(move |_, window, cx| open_cb(index, window, cx));
                }
                if let Some(remove_cb) = remove_cb {
                    row = row.child(
                        div()
                            .id(remove_id)
                            .track_focus(&remove_handle)
                            .tab_stop(true)
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .size(px(ROW_DIFF))
                            .rounded(px(RADIUS_SM))
                            .text_color(hsla(p.ink_muted))
                            .cursor_pointer()
                            .hover(move |style| style.bg(remove_hover_bg).text_color(hover_ink))
                            .focus_visible(move |style| focus::inset_ring(style, ring))
                            .on_click(move |_, window, cx| {
                                cx.stop_propagation();
                                remove_cb(index, window, cx);
                            })
                            .child("×"),
                    );
                }
                list = list.child(row);
            }

            div()
                .flex()
                .flex_col()
                .gap(px(SPACE_2))
                .child(Divider::horizontal())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(SPACE_2))
                        .pt(px(SPACE_1))
                        .child(SectionLabel::new("Recent repositories"))
                        .children(on_clear_recent.map(|on_clear| {
                            Button::new("welcome-clear-recent", "Clear")
                                .ghost()
                                .compact()
                                .on_click(move |_, window, cx| on_clear(window, cx))
                        })),
                )
                .child(list)
        });

        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .p(px(SPACE_8))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_4))
                    .w(px(DIALOG))
                    .p(px(SPACE_6))
                    .rounded(px(RADIUS_LG))
                    .bg(hsla(p.bg_panel))
                    .border_1()
                    .border_color(hsla(p.line))
                    .shadow(shadows(p.shadow_overlay))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(SPACE_2))
                            .child(
                                div()
                                    .text_style(&TEXT_DISPLAY, cx)
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
                            ),
                    )
                    .children(self.notices)
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(SPACE_2))
                            .pt(px(SPACE_2))
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
                    )
                    .children(recent_section),
            )
    }
}
