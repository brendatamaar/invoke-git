//! Virtualized blame and commit lists for file history.

use std::{ops::Range, rc::Rc, sync::Arc};

use gpui::{
    App, ElementId, HighlightStyle, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, StatefulInteractiveElement, Styled, StyledText, UniformListScrollHandle, Window,
    div, px, uniform_list,
};
use tokens::{
    RADIUS_SM, ROW_DIFF, ROW_LIST, SPACE_2, SPACE_4, SPACE_8, TEXT_CODE_SMALL, TEXT_SMALL,
};

use crate::commit_detail::short_hash;
use crate::convert::hsla;
use crate::scrollbar::Scrollbar;
use crate::{Button, TextStyleExt, ThemeExt, focus};

pub struct BlameRow {
    pub commit: Option<SharedString>,
    pub author: SharedString,
    pub date: SharedString,
    pub line: u32,
    pub text: SharedString,
}

pub struct BlameData {
    rows: Vec<BlameRow>,
    starts: Vec<usize>,
    spans: Vec<highlight::Span>,
}

impl BlameData {
    /// Format and highlight away from the UI thread. Styling a rendered row
    /// only visits spans intersecting that row's byte range.
    pub fn new(path: &str, rows: Vec<BlameRow>, cancelled: &dyn Fn() -> bool) -> Self {
        let mut data = Self {
            rows,
            starts: Vec::new(),
            spans: Vec::new(),
        };
        let first_line = data.rows.first().map(|row| row.text.as_ref());
        if let Some(language) = highlight::detect(path, None, first_line) {
            let mut source = String::new();
            for row in &data.rows {
                if cancelled() || source.len() + row.text.len() >= highlight::MAX_BYTES {
                    return data;
                }
                data.starts.push(source.len());
                source.push_str(&row.text);
                source.push('\n');
            }
            data.spans = highlight::highlight(language, &source, cancelled).unwrap_or_default();
        }
        data
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn line_spans(
        &self,
        ix: usize,
    ) -> impl Iterator<Item = (Range<usize>, highlight::Capture)> + '_ {
        let start = self.starts.get(ix).copied().unwrap_or_default();
        let end = start + self.rows[ix].text.len();
        self.starts
            .get(ix)
            .filter(|_| start < end)
            .into_iter()
            .flat_map(move |_| highlight::spans_in(&self.spans, start..end))
            .map(move |span| {
                (
                    span.start.max(start) - start..span.end.min(end) - start,
                    span.capture,
                )
            })
    }
}

type OpenCommit = Rc<dyn Fn(SharedString, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct BlameView {
    data: Arc<BlameData>,
    scroll: UniformListScrollHandle,
    open: OpenCommit,
}

impl BlameView {
    pub fn new(
        data: Arc<BlameData>,
        scroll: UniformListScrollHandle,
        open: impl Fn(SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            data,
            scroll,
            open: Rc::new(open),
        }
    }
}

impl RenderOnce for BlameView {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let data = self.data;
        let open = self.open;
        let count = data.rows.len();
        let list = uniform_list("blame-lines", count, move |range, _, cx| {
            let p = cx.palette();
            let theme = cx.ui_theme().theme();
            range
                .map(|ix| {
                    let row = &data.rows[ix];
                    let highlights = data.line_spans(ix).map(|(range, capture)| {
                        (
                            range,
                            HighlightStyle {
                                color: Some(hsla(capture.token(theme))),
                                ..Default::default()
                            },
                        )
                    });
                    let commit = match &row.commit {
                        Some(id) => {
                            let id = id.clone();
                            let open = open.clone();
                            Button::new(("blame-commit", ix), short_hash(&id).to_owned())
                                .ghost()
                                .on_click(move |_, window, cx| open(id.clone(), window, cx))
                                .into_any_element()
                        }
                        None => div().child("Uncommitted").into_any_element(),
                    };
                    div()
                        .flex()
                        .items_center()
                        .h(px(ROW_DIFF))
                        .gap(px(SPACE_2))
                        .px(px(SPACE_2))
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(div().flex_none().w(px(SPACE_8 * 3.)).child(commit))
                        .child(
                            div()
                                .flex_none()
                                .w(px(SPACE_8 * 4.))
                                .truncate()
                                .child(row.author.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(SPACE_8 * 6.))
                                .truncate()
                                .child(row.date.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(SPACE_4 * 3.))
                                .text_right()
                                .child(row.line.to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_style(&TEXT_CODE_SMALL, cx)
                                .text_color(hsla(p.ink))
                                .child(
                                    StyledText::new(row.text.clone()).with_highlights(highlights),
                                ),
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
                "blame-scrollbar",
                self.scroll,
                count,
                ROW_DIFF,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multiline_capture_is_clipped_to_each_nonempty_row() {
        let rows = ["/* α", "", "β */"]
            .into_iter()
            .enumerate()
            .map(|(ix, text)| BlameRow {
                commit: None,
                author: "".into(),
                date: "".into(),
                line: (ix + 1) as u32,
                text: text.into(),
            })
            .collect();
        let data = BlameData::new("sample.rs", rows, &|| false);
        assert_eq!(
            data.line_spans(0).next(),
            Some((0..5, highlight::Capture::Comment))
        );
        assert_eq!(data.line_spans(1).count(), 0);
        assert_eq!(
            data.line_spans(2).next(),
            Some((0..5, highlight::Capture::Comment))
        );
    }
}

#[derive(Clone)]
pub struct CommitListRow {
    pub id: SharedString,
    pub subject: SharedString,
    pub details: SharedString,
}

#[derive(IntoElement)]
pub struct CommitList {
    rows: Arc<[CommitListRow]>,
    scroll: UniformListScrollHandle,
    open: OpenCommit,
}

impl CommitList {
    pub fn new(
        rows: Arc<[CommitListRow]>,
        scroll: UniformListScrollHandle,
        open: impl Fn(SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            rows,
            scroll,
            open: Rc::new(open),
        }
    }
}

impl RenderOnce for CommitList {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let rows = self.rows;
        let open = self.open;
        let count = rows.len();
        let list = uniform_list("commit-list", count, move |range, window, cx| {
            let p = cx.palette();
            let hover = hsla(p.bg_hover);
            let ring = focus::ring_color(cx);
            range
                .map(|ix| {
                    let row = &rows[ix];
                    let id = row.id.clone();
                    let open = open.clone();
                    let element_id = ElementId::from(("commit-list-row", ix));
                    let handle = focus::handle(&element_id, window, cx);
                    div()
                        .id(element_id)
                        .track_focus(&handle)
                        .tab_stop(true)
                        .cursor_pointer()
                        .rounded(px(RADIUS_SM))
                        .hover(move |style| style.bg(hover))
                        .focus_visible(move |style| focus::ring(style, ring))
                        .on_click(move |_, window, cx| open(id.clone(), window, cx))
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
                                .text_style(&TEXT_CODE_SMALL, cx)
                                .child(short_hash(&row.id).to_owned()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .child(row.subject.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_color(hsla(p.ink_muted))
                                .child(row.details.clone()),
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
                "commit-list-scrollbar",
                self.scroll,
                count,
                ROW_LIST,
            ))
    }
}
