//! Virtualized local reflog entries. Selection identifies the entry by row
//! in the snapshot rendered by the pane.

use std::{rc::Rc, sync::Arc};

use gpui::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window, div, px, uniform_list,
};
use invoke_core::ReflogEntry;
use tokens::{
    RADIUS_MD, RADIUS_SM, ROW_LIST, SPACE_1, SPACE_2, SPACE_8, TEXT_CODE_SMALL, TEXT_SMALL,
};

use crate::{Pill, PillTone, TextStyleExt, ThemeExt, convert::hsla, focus, scrollbar::Scrollbar};

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
                    let is_selected = selected == Some(index);
                    let tone = if action.starts_with("commit") || action.starts_with("checkout") {
                        PillTone::Accent
                    } else if action.starts_with("reset") || action.starts_with("rebase") {
                        PillTone::Warning
                    } else {
                        PillTone::Neutral
                    };
                    let mut row = div()
                        .id(id)
                        .track_focus(&handle)
                        .tab_stop(true)
                        .relative()
                        .cursor_pointer()
                        .rounded(px(RADIUS_MD))
                        .focus_visible(move |style| focus::ring(style, ring))
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
                                .text_color(hsla(p.ink_muted))
                                .child(format!("→ {}", crate::short_hash(entry.new_oid.as_hex()))),
                        )
                        .child(
                            Pill::new(action.to_owned())
                                .tone(tone)
                                .compact()
                                .max_w(SPACE_8 * 5.),
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
                                .text_color(hsla(p.ink_muted))
                                .child(entry.actor.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(hsla(p.ink_muted))
                                .child(crate::time::format_date_time(entry.seconds, entry.offset)),
                        );
                    row = if is_selected {
                        row.bg(hsla(p.bg_raised)).child(
                            div()
                                .absolute()
                                .left(px(0.))
                                .top(px(SPACE_1))
                                .bottom(px(SPACE_1))
                                .w(px(focus::ACCENT_BAR_W))
                                .rounded(px(RADIUS_SM))
                                .bg(hsla(p.accent)),
                        )
                    } else {
                        row.hover(move |style| style.bg(hover))
                    };
                    row
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
            .px(px(SPACE_2))
            .child(list)
            .child(Scrollbar::new(
                "reflog-scrollbar",
                self.scroll,
                count,
                ROW_LIST,
            ))
    }
}
