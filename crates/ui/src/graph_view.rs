//! The commit graph view (backlog task 18): a fixed-height virtualized list
//! with columns for the graph, subject (with ref labels), author, date and
//! short hash. Each visible row draws its own slice of the lanes with a
//! canvas, so the cost depends only on the rows on screen.
//!
//! The view holds the lane layout (`graph::Layout`) and renders it; it reads
//! nothing from disk. Pages of commits arrive with [`GraphView::push_page`];
//! the subject, author and date of the visible rows (and a buffer) are asked
//! for with [`GraphEvent::NeedMeta`] and delivered with
//! [`GraphView::set_meta`], so the UI thread never waits for a commit read.

use std::collections::HashMap;
use std::fmt::Display;
use std::ops::Range;

use gpui::{
    AnyElement, BorderStyle, Bounds, Context, Corners, Div, Edges, EventEmitter, FontWeight, Hsla,
    InteractiveElement, IntoElement, MouseButton, ParentElement, PathBuilder, Pixels, Point,
    Render, SharedString, StatefulInteractiveElement, Styled, UniformListScrollHandle, Window,
    canvas, div, fill, point, px, quad, size, uniform_list,
};
use graph::{Edge, Layout, Row};
use tokens::{
    LANE, RADIUS_SM, ROW_GRAPH, SPACE_2, SPACE_4, TEXT_BODY, TEXT_CODE_SMALL, TEXT_LABEL,
    TEXT_SMALL,
};

use crate::convert::hsla;
use crate::ref_labels::{LabelKind, RefLabel};
use crate::scrollbar::Scrollbar;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// More lanes than this are collapsed into a `+N` marker (spec: "capped at
/// 16 lanes").
pub const MAX_LANES: u16 = 16;
const GRAPH_PAD: f32 = SPACE_2;
const STROKE: f32 = 2.0;
const DOT_R: f32 = 5.0;
const AUTHOR_W: f32 = 110.0;
const DATE_W: f32 = 96.0;
const HASH_W: f32 = 64.0;
/// Rows whose metadata is requested together.
const CHUNK: usize = 128;
/// How far beyond the visible rows metadata is requested.
const AHEAD: usize = 512;
/// At most this many labels before a subject; the rest show as `+n`.
const MAX_LABELS: usize = 3;

/// Room for the `+N` marker after a capped graph column.
const MARKER_W: f32 = 40.0;

/// Width of the graph column for `lanes` columns: at most [`MAX_LANES`]
/// lanes are drawn, and a history with more gets room for the `+N` marker.
pub fn graph_width(lanes: u16) -> f32 {
    let marker = if lanes > MAX_LANES { MARKER_W } else { 0.0 };
    f32::from(lanes.min(MAX_LANES)) * LANE + 2.0 * GRAPH_PAD + marker
}

/// The x of a lane's centre, from the left of the graph column.
pub fn lane_x(column: u16) -> f32 {
    GRAPH_PAD + f32::from(column) * LANE + LANE / 2.0
}

/// What a row shows besides its graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowMeta {
    pub subject: SharedString,
    pub author: SharedString,
    pub date: SharedString,
}

pub enum GraphEvent {
    /// These rows are (or are about to be) on screen and have no metadata.
    NeedMeta(Range<usize>),
    /// The user selected a row.
    Selected(usize),
    RowMenu {
        row: usize,
        position: Point<Pixels>,
    },
}

enum Status {
    Loading,
    Ready,
    /// The walk finished and found no commits.
    Empty,
    Failed(SharedString),
}

pub struct GraphView<I> {
    layout: Layout<I>,
    ids: Vec<I>,
    meta: Vec<Option<RowMeta>>,
    /// One flag per [`CHUNK`] rows: metadata asked for.
    requested: Vec<bool>,
    labels: HashMap<String, Vec<RefLabel>>,
    head: Option<I>,
    selected: Option<(usize, I)>,
    hover: Option<usize>,
    /// A refresh walk is running: the old graph stays until its first page.
    refreshing: bool,
    status: Status,
    /// The widest row so far, in columns: the graph column's width.
    max_width: u16,
    scroll: UniformListScrollHandle,
}

impl<I: Copy + Eq + Display + 'static> EventEmitter<GraphEvent> for GraphView<I> {}

impl<I: Copy + Eq + Display + 'static> Default for GraphView<I> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: Copy + Eq + Display + 'static> GraphView<I> {
    pub fn new() -> Self {
        Self {
            layout: Layout::new(),
            ids: Vec::new(),
            meta: Vec::new(),
            requested: Vec::new(),
            labels: HashMap::new(),
            head: None,
            selected: None,
            hover: None,
            refreshing: false,
            status: Status::Loading,
            max_width: 0,
            scroll: UniformListScrollHandle::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn id_at(&self, row: usize) -> Option<&I> {
        self.ids.get(row)
    }

    pub fn selected(&self) -> Option<(usize, &I)> {
        self.selected.as_ref().map(|(row, id)| (*row, id))
    }

    /// Starts over (a new repo, or the first load): everything is dropped.
    pub fn begin(&mut self, cx: &mut Context<Self>) {
        *self = Self {
            labels: std::mem::take(&mut self.labels),
            head: self.head,
            scroll: self.scroll.clone(),
            ..Self::new()
        };
        cx.notify();
    }

    /// A refresh walk is starting: the graph on screen stays until its first
    /// page arrives, and then the scroll position stays on the same commit.
    pub fn begin_refresh(&mut self) {
        self.refreshing = true;
    }

    /// Adds the next page of commits (children first).
    pub fn push_page(&mut self, commits: &[graph::Commit<I>], cx: &mut Context<Self>) {
        let mut anchor = None;
        if self.refreshing {
            self.refreshing = false;
            anchor = self.top_row().and_then(|row| self.ids.get(row).copied());
            self.layout = Layout::new();
            self.ids.clear();
            self.meta.clear();
            self.requested.clear();
            self.max_width = 0;
        }
        let before = self.layout.len();
        self.layout.push_page(commits);
        self.ids.extend(commits.iter().map(|c| c.id));
        self.meta.resize(self.ids.len(), None);
        self.requested.resize(self.ids.len().div_ceil(CHUNK), false);
        for row in &self.layout.rows()[before..] {
            self.max_width = self.max_width.max(row.width);
        }
        if let Some((_, id)) = self.selected {
            self.selected = self
                .ids
                .iter()
                .position(|candidate| *candidate == id)
                .map(|row| (row, id));
        }
        if let Some(anchor) = anchor {
            let row = self.ids.iter().position(|id| *id == anchor).unwrap_or(0);
            self.scroll_to_row(row);
        }
        self.status = Status::Ready;
        cx.notify();
    }

    /// The walk is over. An empty history shows a message.
    pub fn finish(&mut self, commits: usize, cx: &mut Context<Self>) {
        if commits == 0 {
            self.status = Status::Empty;
        }
        cx.notify();
    }

    pub fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        if self.ids.is_empty() {
            self.status = Status::Failed(message.into());
        }
        cx.notify();
    }

    /// Delivers metadata for rows starting at `start`.
    pub fn set_meta(&mut self, start: usize, metas: Vec<Option<RowMeta>>, cx: &mut Context<Self>) {
        for (offset, meta) in metas.into_iter().enumerate() {
            if let Some(slot) = self.meta.get_mut(start + offset) {
                *slot = meta;
            }
        }
        cx.notify();
    }

    /// The labels (by commit id as hex) and the commit `HEAD` is on.
    pub fn set_refs(
        &mut self,
        labels: HashMap<String, Vec<RefLabel>>,
        head: Option<I>,
        cx: &mut Context<Self>,
    ) {
        self.labels = labels;
        self.head = head;
        cx.notify();
    }

    pub fn select(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(id) = self.ids.get(row) {
            self.selected = Some((row, *id));
            cx.emit(GraphEvent::Selected(row));
            cx.notify();
        }
    }

    fn set_hover(&mut self, row: usize, hovered: bool, cx: &mut Context<Self>) {
        let next = if hovered {
            Some(row)
        } else if self.hover == Some(row) {
            None
        } else {
            self.hover
        };
        if next != self.hover {
            self.hover = next;
            cx.notify();
        }
    }

    fn top_row(&self) -> Option<usize> {
        let top = -f32::from(self.scroll.0.borrow().base_handle.offset().y);
        (!self.ids.is_empty())
            .then(|| ((top / ROW_GRAPH).floor().max(0.0) as usize).min(self.ids.len() - 1))
    }

    fn scroll_to_row(&self, row: usize) {
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-(row as f32) * ROW_GRAPH)));
    }

    /// Asks for the metadata of the rows around `range` that nobody asked
    /// for yet.
    fn request_meta(&mut self, range: &Range<usize>, cx: &mut Context<Self>) {
        let from = range.start.saturating_sub(AHEAD) / CHUNK;
        let to = ((range.end + AHEAD).min(self.ids.len())).div_ceil(CHUNK);
        for chunk in from..to.min(self.requested.len()) {
            if !self.requested[chunk] {
                self.requested[chunk] = true;
                let start = chunk * CHUNK;
                cx.emit(GraphEvent::NeedMeta(
                    start..(start + CHUNK).min(self.ids.len()),
                ));
            }
        }
    }

    fn render_header(&self, graph_w: f32, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let cell = |text: &'static str, width: Option<f32>| {
            let base = div().flex_none().overflow_hidden();
            match width {
                Some(w) => base.w(px(w)),
                None => base.flex_1().min_w(px(0.)),
            }
            .child(text)
        };
        div()
            .flex()
            .flex_none()
            .items_center()
            .h(px(ROW_GRAPH))
            .w_full()
            .pl(px(SPACE_2))
            .pr(px(SPACE_2 + crate::scrollbar::WIDTH))
            .border_b_1()
            .border_color(hsla(p.line))
            .text_style(&TEXT_LABEL, cx)
            .text_color(hsla(p.ink_muted))
            .child(cell("GRAPH", Some(graph_w)))
            .child(cell("SUBJECT", None))
            .child(cell("AUTHOR", Some(AUTHOR_W)))
            .child(cell("DATE", Some(DATE_W)))
            .child(cell("HASH", Some(HASH_W)))
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        graph_w: f32,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.request_meta(&range, cx);
        let p = cx.palette();
        let lane_colors: [Hsla; 8] = std::array::from_fn(|i| hsla(p.lane(i)));
        let palette = Paint {
            lanes: lane_colors,
            panel: hsla(p.bg_panel),
            accent: hsla(p.accent),
        };
        let hover_color = self
            .hover
            .and_then(|row| self.layout.row(row))
            .map(|row| row.color);
        let this = cx.entity();
        range
            .map(|index| {
                let Some(row) = self.layout.row(index).copied() else {
                    return div().h(px(ROW_GRAPH)).into_any_element();
                };
                let id = self.ids[index];
                let is_head = self.head == Some(id);
                let is_selected = self.selected.as_ref().is_some_and(|(r, _)| *r == index);
                let dot = Dot {
                    row,
                    head: is_head,
                    selected: is_selected,
                    dim: hover_color,
                };
                let edges_in: Vec<Edge> = self.layout.edges_in(&row).to_vec();
                let edges_out: Vec<Edge> = self.layout.edges_out(&row).to_vec();
                let passing = self.layout.passing(index).unwrap_or_default();
                let key = id.to_string();
                let labels = self.labels.get(&key).cloned().unwrap_or_default();
                let meta = self.meta[index].clone();
                let (select, hover) = (this.clone(), this.clone());
                let menu = this.clone();
                let mut line = div()
                    .id(("graph-row", index))
                    .flex()
                    .items_center()
                    .h(px(ROW_GRAPH))
                    .w_full()
                    .px(px(SPACE_2))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| select.update(cx, |view, cx| view.select(index, cx)))
                    .on_mouse_down(MouseButton::Right, move |event, _, cx| {
                        menu.update(cx, |view, cx| {
                            view.select(index, cx);
                            cx.emit(GraphEvent::RowMenu {
                                row: index,
                                position: event.position,
                            });
                        });
                    })
                    .on_hover(move |hovered: &bool, _, cx| {
                        hover.update(cx, |view, cx| view.set_hover(index, *hovered, cx))
                    });
                line = if is_selected {
                    line.bg(hsla(p.bg_raised))
                } else {
                    let hover_fill = hsla(p.bg_hover);
                    line.hover(move |style| style.bg(hover_fill))
                };
                let merge_ink = if row.merge { p.ink_muted } else { p.ink };
                line.child(graph_cell(
                    graph_w, palette, dot, edges_in, edges_out, passing, row.width, cx,
                ))
                .child(subject_cell(&labels, meta.as_ref(), merge_ink, p, cx))
                .child(text_cell(
                    AUTHOR_W,
                    meta.as_ref().map(|m| m.author.clone()),
                    p.ink_muted,
                    cx,
                ))
                .child(text_cell(
                    DATE_W,
                    meta.as_ref().map(|m| m.date.clone()),
                    p.ink_muted,
                    cx,
                ))
                .child(
                    div()
                        .flex_none()
                        .w(px(HASH_W))
                        .text_style(&TEXT_CODE_SMALL, cx)
                        .text_color(hsla(p.ink_faint))
                        .child(key[..key.len().min(7)].to_owned()),
                )
                .into_any_element()
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
struct Paint {
    lanes: [Hsla; 8],
    panel: Hsla,
    accent: Hsla,
}

#[derive(Clone, Copy)]
struct Dot {
    row: Row,
    head: bool,
    selected: bool,
    /// When a row is hovered, lanes of other colours fade.
    dim: Option<u8>,
}

#[allow(clippy::too_many_arguments)]
fn graph_cell(
    width: f32,
    paint: Paint,
    dot: Dot,
    edges_in: Vec<Edge>,
    edges_out: Vec<Edge>,
    passing: Vec<(u16, u8)>,
    row_width: u16,
    cx: &gpui::App,
) -> Div {
    let p = cx.palette();
    let mut cell = div().flex_none().relative().w(px(width)).h_full().child(
        canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                paint_lanes(window, bounds, paint, dot, &edges_in, &edges_out, &passing)
            },
        )
        .absolute()
        .size_full(),
    );
    if row_width > MAX_LANES {
        cell = cell.child(
            div()
                .absolute()
                .right(px(0.))
                .top(px(0.))
                .h_full()
                .flex()
                .items_center()
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink_faint))
                .child(format!("+{}", row_width - MAX_LANES)),
        );
    }
    cell
}

fn lane_color(paint: &Paint, dim: Option<u8>, color: u8) -> Hsla {
    let base = paint.lanes[usize::from(color) % 8];
    match dim {
        Some(hovered) if hovered != color => base.opacity(0.3),
        _ => base,
    }
}

fn paint_lanes(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    paint: Paint,
    dot: Dot,
    edges_in: &[Edge],
    edges_out: &[Edge],
    passing: &[(u16, u8)],
) {
    let row = dot.row;
    let x = |col: u16| bounds.origin.x + px(lane_x(col));
    let top = bounds.origin.y;
    let mid = top + px(ROW_GRAPH / 2.0);
    let bottom = top + px(ROW_GRAPH);
    let drawn = |col: u16| col < MAX_LANES;
    let colour = |c: u8| lane_color(&paint, dot.dim, c);
    let vline = |window: &mut Window, col: u16, from: Pixels, to: Pixels, c: u8| {
        let origin = point(x(col) - px(STROKE / 2.0), from);
        window.paint_quad(fill(
            Bounds::new(origin, size(px(STROKE), to - from)),
            colour(c),
        ));
    };
    let curve = |window: &mut Window, from: Point<Pixels>, to: Point<Pixels>, c: u8| {
        let mut path = PathBuilder::stroke(px(STROKE));
        path.move_to(from);
        path.curve_to(to, point(from.x, to.y));
        if let Ok(path) = path.build() {
            window.paint_path(path, colour(c));
        }
    };
    for &(col, c) in passing {
        if drawn(col) {
            vline(window, col, top, bottom, c);
        }
    }
    for edge in edges_in {
        if !drawn(edge.column) {
            continue;
        }
        if edge.column == row.column {
            vline(window, row.column, top, mid, edge.color);
        } else if drawn(row.column) {
            curve(
                window,
                point(x(edge.column), top),
                point(x(row.column), mid),
                edge.color,
            );
        }
    }
    for edge in edges_out {
        if !drawn(edge.column) {
            continue;
        }
        if edge.column == row.column {
            vline(window, row.column, mid, bottom, edge.color);
        } else if drawn(row.column) {
            curve(
                window,
                point(x(row.column), mid),
                point(x(edge.column), bottom),
                edge.color,
            );
        }
    }
    if !drawn(row.column) {
        return;
    }
    // A shallow boundary: the lane ends in a short faded stub.
    if row.truncated {
        let origin = point(x(row.column) - px(STROKE / 2.0), mid);
        window.paint_quad(fill(
            Bounds::new(origin, size(px(STROKE), px(8.0))),
            colour(row.color).opacity(0.35),
        ));
    }
    let bounds = Bounds::new(
        point(x(row.column) - px(DOT_R), mid - px(DOT_R)),
        size(px(DOT_R * 2.0), px(DOT_R * 2.0)),
    );
    let lane = colour(row.color);
    // HEAD is a ring in the accent colour; merges are hollow; the selected
    // row's dot takes the accent colour.
    let (background, border, border_color) = if dot.head {
        (paint.panel, 2.0, paint.accent)
    } else if dot.selected {
        (paint.accent, 0.0, paint.accent)
    } else if row.merge {
        (paint.panel, STROKE, lane)
    } else {
        (lane, 0.0, lane)
    };
    window.paint_quad(quad(
        bounds,
        Corners::all(px(DOT_R)),
        background,
        Edges::all(px(border)),
        border_color,
        BorderStyle::Solid,
    ));
}

fn text_cell(width: f32, text: Option<SharedString>, ink: tokens::Rgba, cx: &gpui::App) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .pr(px(SPACE_2))
        .truncate()
        .text_style(&TEXT_SMALL, cx)
        .text_color(hsla(ink))
        .child(text.unwrap_or_default())
}

fn label_pill(label: &RefLabel, p: &tokens::Palette, cx: &gpui::App) -> Div {
    let base = div()
        .flex_none()
        .flex()
        .items_center()
        .h(px(20.))
        .px(px(6.))
        .rounded(px(RADIUS_SM))
        .text_style(&TEXT_SMALL, cx);
    let line = hsla(p.line);
    match label.kind {
        LabelKind::Current => base
            .bg(hsla(p.accent_soft))
            .text_color(hsla(p.accent))
            .font_weight(FontWeight(600.0)),
        LabelKind::Local => base
            .bg(hsla(p.bg_raised))
            .text_color(hsla(p.ink))
            .border_1()
            .border_color(line)
            .font_weight(FontWeight(500.0)),
        LabelKind::Remote => base
            .text_color(hsla(p.ink_muted))
            .border_1()
            .border_color(line),
        LabelKind::Tag => base
            .text_color(hsla(p.ink_muted))
            .border_1()
            .border_dashed()
            .border_color(hsla(p.line_control)),
    }
    .child(label.name.clone())
}

fn subject_cell(
    labels: &[RefLabel],
    meta: Option<&RowMeta>,
    ink: tokens::Rgba,
    p: &tokens::Palette,
    cx: &gpui::App,
) -> Div {
    let mut cell = div()
        .flex()
        .flex_1()
        .min_w(px(0.))
        .items_center()
        .gap(px(6.))
        .overflow_hidden()
        .text_style(&TEXT_BODY, cx);
    for label in labels.iter().take(MAX_LABELS) {
        cell = cell.child(label_pill(label, p, cx));
    }
    if labels.len() > MAX_LABELS {
        cell = cell.child(
            div()
                .flex_none()
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink_muted))
                .child(format!("+{}", labels.len() - MAX_LABELS)),
        );
    }
    cell.child(
        div()
            .flex_1()
            .min_w(px(0.))
            .truncate()
            .text_color(hsla(ink))
            .child(meta.map(|m| m.subject.clone()).unwrap_or_default()),
    )
}

impl<I: Copy + Eq + Display + 'static> Render for GraphView<I> {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let graph_w = graph_width(self.max_width.max(1));
        let body: AnyElement = match &self.status {
            Status::Loading => message("Reading the history…", cx),
            Status::Empty => message("No commits yet", cx),
            Status::Failed(text) => message(text.clone(), cx),
            Status::Ready => {
                let list = uniform_list(
                    "graph-rows",
                    self.ids.len(),
                    cx.processor(move |this, range, _window, cx| {
                        this.render_rows(range, graph_w, cx)
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .h_full();
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(list)
                    .child(Scrollbar::new(
                        "graph-scrollbar",
                        self.scroll.clone(),
                        self.ids.len(),
                        ROW_GRAPH,
                    ))
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(hsla(p.bg_panel))
            .child(self.render_header(graph_w, cx))
            .child(body)
    }
}

fn message(text: impl Into<SharedString>, cx: &gpui::App) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .px(px(SPACE_4))
        .text_style(&TEXT_BODY, cx)
        .text_color(hsla(cx.palette().ink_muted))
        .child(text.into())
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_graph_column_grows_with_the_lanes_up_to_the_cap() {
        assert_eq!(graph_width(1), LANE + 2.0 * GRAPH_PAD);
        assert_eq!(graph_width(16), 16.0 * LANE + 2.0 * GRAPH_PAD);
        // Beyond the cap the width stops growing and a marker gets its room.
        assert_eq!(graph_width(691), graph_width(17));
        assert_eq!(graph_width(17), graph_width(16) + MARKER_W);
    }

    #[test]
    fn lanes_are_one_lane_width_apart() {
        assert_eq!(lane_x(1) - lane_x(0), LANE);
        assert_eq!(lane_x(0), GRAPH_PAD + LANE / 2.0);
    }
}
