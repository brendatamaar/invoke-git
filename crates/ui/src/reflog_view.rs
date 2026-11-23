//! Virtualized local reflog entries. Selection identifies the entry by row
//! in the snapshot rendered by the pane.

use std::{rc::Rc, sync::Arc};

use gpui::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window, div, px, uniform_list,
};
use invoke_core::ReflogEntry;
use tokens::{RADIUS_SM, ROW_LIST, SPACE_2, SPACE_8, TEXT_CODE_SMALL, TEXT_SMALL};

use crate::{Pill, TextStyleExt, ThemeExt, convert::hsla, focus, scrollbar::Scrollbar};

type Select = Rc<dyn Fn(usize, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct ReflogView {
    entries: Arc<[ReflogEntry]>,
    selection: Option<usize>,
    scroll: UniformListScrollHandle,
    select: Select,
}

impl ReflogView {
    pub fn new(
        entries: Arc<[ReflogEntry]>,
        selection: Option<usize>,
        scroll: UniformListScrollHandle,
        select: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            entries,
            selection,
            scroll,
            select: Rc::new(select),
        }
    }
}

impl RenderOnce for ReflogView {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let count = self.entries.len();
        let entries = self.entries;
        let selected = self.selection;
        let select = self.select;
        let list = uniform_list("reflog-entries", count, move |range, window, cx| {
            let p = cx.palette();
            let ring = focus::ring_color(cx);
            range
                .map(|index| {
                    let entry = &entries[index];
                    let (action, detail) = entry.split_message();
                    let select = select.clone();
                    let id = ElementId::from(("reflog-entry", index));
                    let handle = focus::handle(&id, window, cx);
                    let hover = hsla(p.bg_hover);
                    div()
                        .id(id)
                        .track_focus(&handle)
                        .tab_stop(true)
                        .cursor_pointer()
                        .rounded(px(RADIUS_SM))
                        .focus_visible(move |style| focus::ring(style, ring))
                        .hover(move |style| style.bg(hover))
                        .bg(hsla(if selected == Some(index) {
                            p.bg_raised
                        } else {
                            p.bg_panel
                        }))
                        .on_click(move |_, window, cx| select(index, window, cx))
                        .flex()
                        .items_center()
                        .h(px(ROW_LIST))
                        .gap(px(SPACE_2))
                        .px(px(SPACE_2))
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink))
                        .child(
                            div()
                                .flex_none()
                                .w(px(SPACE_8 * 4.))
                                .truncate()
                                .text_style(&TEXT_CODE_SMALL, cx)
                                .child(entry.selector.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_style(&TEXT_CODE_SMALL, cx)
                                .child(crate::short_hash(entry.new_oid.as_hex()).to_owned()),
                        )
                        .child(
                            div()
                                .max_w(px(SPACE_8 * 5.))
                                .overflow_hidden()
                                .child(Pill::new(action.to_owned())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .child(detail.to_owned()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(SPACE_8 * 3.))
                                .truncate()
                                .child(entry.actor.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(hsla(p.ink_muted))
                                .child(crate::time::format_date_time(entry.seconds, entry.offset)),
                        )
                })
                .collect()
        })
        .track_scroll(&self.scroll)
        .flex_1()
        .h_full();
        div()
            .flex()
            .size_full()
            .min_h(px(0.))
            .child(list)
            .child(Scrollbar::new(
                "reflog-scrollbar",
                self.scroll,
                count,
                ROW_LIST,
            ))
    }
}
