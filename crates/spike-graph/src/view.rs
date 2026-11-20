//! The commit graph view: virtualized 28 px rows with a custom-painted lane
//! column, lazily read metadata, a custom scrollbar and the measurement readout.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use gix::ObjectId;
use gpui::{
    BorderStyle, Bounds, Context, Corners, DispatchPhase, Div, Edges, Hsla, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, SharedString,
    UniformListScrollHandle, Window, canvas, div, fill, point, prelude::*, px, quad, rgb, size,
    uniform_list,
};

use crate::layout::{self, Edge, Lanes, Row};
use crate::meta::{self, Meta};
use crate::stats::FrameStats;
use crate::walk::{self, WalkEvent};

/// Dark-theme values from docs/design/tokens.json. Spike only: the real app
/// reads these from the `tokens` crate.
mod color {
    pub const BG_APP: u32 = 0x0e0f11;
    pub const BG_PANEL: u32 = 0x15161a;
    pub const LINE: u32 = 0x2a2d33;
    pub const LINE_CONTROL: u32 = 0x686d77;
    pub const INK: u32 = 0xeceef1;
    pub const INK_MUTED: u32 = 0xa0a4ac;
    pub const INK_FAINT: u32 = 0x8a8f98;
    pub const ACCENT: u32 = 0x8b95fa;
    pub const REMOVE_INK: u32 = 0xff9196;
    pub const LANES: [u32; 8] = [
        0x8b95fa, 0x3fc7b5, 0xf2b350, 0xf483b3, 0x7bd88f, 0xf59d5f, 0x62b8f5, 0xc690f5,
    ];
}

/// `row-graph` and `lane` sizes.
const ROW_H: f32 = 28.0;
const LANE_W: f32 = 14.0;
const MAX_LANES: u16 = 16;
const GRAPH_PAD: f32 = 8.0;
const DOT_R: f32 = 4.0;
const STROKE: f32 = 1.5;
const TOOLBAR_H: f32 = 40.0;
const SCROLLBAR_W: f32 = 12.0;
const MIN_THUMB: f32 = 24.0;
/// The gate scrolls this deep.
const SWEEP_DEPTH: usize = 100_000;
/// Rows read ahead (and behind) the visible ones in the background.
const PREFETCH: usize = 1_000;
const CODE_FONT: &str = "Cascadia Mono";
const UI_FONT: &str = "Segoe UI";

/// Automated scroll for repeatable measurement: `--sweep SECONDS` scrolls from
/// the newest commit to 100k deep once enough rows are loaded, then quits.
#[derive(Clone, Copy)]
pub enum Sweep {
    Off,
    Waiting { seconds: f32 },
    Running { start: Instant, seconds: f32 },
    Done,
}

enum Phase {
    Loading,
    Failed(SharedString),
    Ready,
}

struct Thumb {
    top: f32,
    height: f32,
    track: f32,
    max_scroll: f32,
}

pub struct GraphView {
    repo_path: SharedString,
    repo: Option<gix::Repository>,
    events: Receiver<WalkEvent>,
    phase: Phase,
    walk_done: Option<(usize, Duration)>,
    first_page_ready: Option<Instant>,
    ids: Vec<ObjectId>,
    rows: Vec<Row>,
    edges: Vec<Edge>,
    checkpoints: Vec<Lanes>,
    meta: Vec<Option<Meta>>,
    /// Rows already sent to the background reader.
    requested: Vec<bool>,
    prefetch: meta::Prefetch,
    meta_reads: u64,
    meta_time: Duration,
    scroll: UniformListScrollHandle,
    stats: Rc<RefCell<FrameStats>>,
    track: Rc<Cell<Bounds<Pixels>>>,
    drag: Option<f32>,
    sweep: Sweep,
}

impl GraphView {
    pub fn new(repo: PathBuf, limit: usize, sweep: Sweep) -> Self {
        let events = walk::spawn(repo.clone(), limit);
        Self {
            repo_path: repo.display().to_string().into(),
            // A second handle for metadata reads on the UI thread.
            repo: gix::open(&repo).ok(),
            events,
            phase: Phase::Loading,
            walk_done: None,
            first_page_ready: None,
            ids: Vec::new(),
            rows: Vec::new(),
            edges: Vec::new(),
            checkpoints: Vec::new(),
            meta: Vec::new(),
            requested: Vec::new(),
            prefetch: meta::spawn_prefetch(repo.clone()),
            meta_reads: 0,
            meta_time: Duration::ZERO,
            scroll: UniformListScrollHandle::new(),
            stats: Rc::new(RefCell::new(FrameStats::new(Instant::now()))),
            track: Rc::new(Cell::new(Bounds::default())),
            drag: None,
            sweep,
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                WalkEvent::Started { commit_graph } => {
                    println!("spike-graph: walk started, commit-graph file used: {commit_graph}");
                }
                WalkEvent::Page(page) => {
                    if self.first_page_ready.is_none() {
                        self.first_page_ready = Some(page.ready_at);
                        println!(
                            "spike-graph: first page of {} ready in {:.1} ms (walk + layout); reached the UI {:.1} ms later",
                            page.ids.len(),
                            page.took.as_secs_f32() * 1000.0,
                            page.ready_at.elapsed().as_secs_f32() * 1000.0
                        );
                    }
                    self.ids.extend(page.ids);
                    self.rows.extend(page.rows);
                    self.edges.extend(page.edges);
                    self.checkpoints.extend(page.checkpoints);
                    self.meta.resize_with(self.rows.len(), || None);
                    self.requested.resize(self.rows.len(), false);
                    self.phase = Phase::Ready;
                }
                WalkEvent::Done { commits, took } => {
                    let lanes = self.rows.iter().map(|r| r.width).max().unwrap_or(0);
                    println!(
                        "spike-graph: walk done, {commits} commits in {:.2} s, {} edges, widest row {lanes} lanes",
                        took.as_secs_f32(),
                        self.edges.len()
                    );
                    self.walk_done = Some((commits, took));
                }
                WalkEvent::Failed(error) => {
                    eprintln!("spike-graph: {error}");
                    if self.rows.is_empty() {
                        self.phase = Phase::Failed(error.into());
                    }
                }
            }
        }
    }

    fn drain_prefetch(&mut self) {
        while let Ok((row, meta)) = self.prefetch.results.try_recv() {
            if let Some(slot @ None) = self.meta.get_mut(row) {
                *slot = Some(meta);
            }
        }
    }

    /// Asks the background reader for rows around `visible` that are neither
    /// loaded nor already requested.
    fn request_prefetch(&mut self, visible: Range<usize>) {
        let window =
            visible.start.saturating_sub(PREFETCH)..(visible.end + PREFETCH).min(self.rows.len());
        let batch: Vec<(usize, ObjectId)> = window
            .filter(|&row| self.meta[row].is_none() && !self.requested[row])
            .map(|row| (row, self.ids[row]))
            .collect();
        if batch.is_empty() {
            return;
        }
        for &(row, _) in &batch {
            self.requested[row] = true;
        }
        self.prefetch.requests.send(batch).ok();
    }

    /// Reads metadata on the UI thread for `rows` that still lack it (the
    /// prefetch has not reached them yet). Returns how many could not be read.
    fn ensure_meta(&mut self, rows: Range<usize>) -> usize {
        let Some(repo) = &self.repo else {
            return rows.len();
        };
        let mut missing = 0;
        for ix in rows {
            if self.meta[ix].is_some() {
                continue;
            }
            let started = Instant::now();
            match meta::read(repo, self.ids[ix]) {
                Ok(meta) => self.meta[ix] = Some(meta),
                Err(error) => {
                    missing += 1;
                    eprintln!("spike-graph: {error}");
                }
            }
            self.meta_time += started.elapsed();
            self.meta_reads += 1;
        }
        missing
    }

    fn scroll_top(&self) -> f32 {
        -f32::from(self.scroll.0.borrow().base_handle.offset().y)
    }

    fn visible_rows(&self) -> Range<usize> {
        let rows = self.rows.len();
        let height = f32::from(self.track.get().size.height).max(ROW_H);
        let first = (self.scroll_top() / ROW_H).floor().max(0.0) as usize;
        let count = (height / ROW_H).ceil() as usize + 1;
        first.min(rows)..(first + count).min(rows)
    }

    fn thumb(&self) -> Option<Thumb> {
        let track = f32::from(self.track.get().size.height);
        let content = self.rows.len() as f32 * ROW_H;
        if track <= 0.0 || content <= track {
            return None;
        }
        let height = (track * track / content).max(MIN_THUMB);
        let max_scroll = content - track;
        let scroll = self.scroll_top().clamp(0.0, max_scroll);
        Some(Thumb {
            top: scroll / max_scroll * (track - height),
            height,
            track,
            max_scroll,
        })
    }

    fn set_scroll(&self, top: f32) {
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-top)));
    }

    fn start_drag(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let Some(thumb) = self.thumb() else {
            return;
        };
        let local = f32::from(y - self.track.get().origin.y);
        let on_thumb = (thumb.top..thumb.top + thumb.height).contains(&local);
        self.drag = Some(if on_thumb {
            local - thumb.top
        } else {
            thumb.height / 2.0
        });
        self.drag_to(y, cx);
    }

    fn drag_to(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let (Some(grab), Some(thumb)) = (self.drag, self.thumb()) else {
            return;
        };
        let local = f32::from(y - self.track.get().origin.y);
        let room = thumb.track - thumb.height;
        let top = (local - grab).clamp(0.0, room);
        self.set_scroll(top / room * thumb.max_scroll);
        cx.notify();
    }

    fn step_sweep(&mut self, now: Instant, cx: &mut Context<Self>) {
        let depth = SWEEP_DEPTH.min(self.rows.len());
        let loaded = self.rows.len() >= SWEEP_DEPTH || self.walk_done.is_some();
        let view = f32::from(self.track.get().size.height);
        let target = (depth as f32 * ROW_H - view).max(0.0);
        match self.sweep {
            Sweep::Waiting { seconds } if loaded && target > 0.0 => {
                self.stats.borrow_mut().reset(now);
                (self.meta_reads, self.meta_time) = (0, Duration::ZERO);
                println!("spike-graph: sweep start, {depth} commits deep in {seconds} s");
                self.sweep = Sweep::Running {
                    start: now,
                    seconds,
                };
            }
            Sweep::Running { start, seconds } => {
                let t = ((now - start).as_secs_f32() / seconds).min(1.0);
                self.set_scroll(t * target);
                if t >= 1.0 {
                    let stats = self.stats.borrow();
                    println!(
                        "spike-graph: sweep done · worst frame {:.1} ms · >50 ms {} · blank {} · UI-thread metadata reads {} ({:.1} µs avg)",
                        stats.worst_interval,
                        stats.long_frames,
                        stats.blank_rows,
                        self.meta_reads,
                        self.meta_avg_us()
                    );
                    self.sweep = Sweep::Done;
                    cx.quit();
                }
            }
            _ => {}
        }
    }

    fn meta_avg_us(&self) -> f32 {
        if self.meta_reads == 0 {
            0.0
        } else {
            self.meta_time.as_secs_f32() * 1e6 / self.meta_reads as f32
        }
    }

    fn graph_width(&self, visible: Range<usize>) -> f32 {
        let lanes = self.rows[visible]
            .iter()
            .map(|r| r.width.max(r.column + 1))
            .max()
            .unwrap_or(1)
            .min(MAX_LANES);
        GRAPH_PAD * 2.0 + f32::from(lanes) * LANE_W
    }

    fn render_rows(&mut self, range: Range<usize>, graph_w: f32) -> Vec<Div> {
        // Rows reach the screen only with their metadata: read any the
        // prefetch missed (a scrollbar jump) before building them.
        let unreadable =
            self.ensure_meta(range.start.min(self.rows.len())..range.end.min(self.rows.len()));
        let mut stats = self.stats.borrow_mut();
        stats.blank_rows += unreadable as u64;
        // Rebuild the lanes passing through the visible rows from the nearest
        // checkpoint; they are not stored per row.
        let mut lanes = if range.start < self.rows.len() {
            layout::lanes_at(range.start, &self.rows, &self.edges, &self.checkpoints)
        } else {
            Vec::new()
        };
        range
            .map(|ix| {
                let (Some(row), Some(Some(meta))) = (self.rows.get(ix), self.meta.get(ix)) else {
                    if ix >= self.rows.len() {
                        stats.blank_rows += 1;
                    }
                    return div().h(px(ROW_H));
                };
                let edges = &self.edges[row.edges.start as usize..row.edges.end as usize];
                let passing: Vec<(u16, u8)> = layout::passing(&lanes, edges)
                    .into_iter()
                    .filter(|(col, _)| *col < MAX_LANES)
                    .collect();
                layout::apply(&mut lanes, edges);
                let hash: String = self.ids[ix].to_hex_with_len(8).to_string();
                let overflow = row.width.saturating_sub(MAX_LANES);
                render_row(
                    ix == 0,
                    row,
                    edges.to_vec(),
                    passing,
                    graph_w,
                    meta,
                    hash,
                    overflow,
                )
            })
            .collect()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let stats = self.stats.borrow();
        let s = stats.last;
        let first_paint = stats
            .first_paint
            .map(|d| format!("{:.0} ms", d.as_secs_f32() * 1000.0))
            .unwrap_or_else(|| "—".into());
        let loaded = match self.walk_done {
            Some((commits, _)) => format!("{commits} commits"),
            None => format!("{} commits, walking…", self.rows.len()),
        };
        let readout = format!(
            "{:.1} fps · frame p95 {:.1} max {:.1} ms · build p95 {:.1} max {:.1} ms · worst {:.1} ms · >50 ms {} · blank {} · sync meta {:.0} µs · first paint {}",
            s.fps,
            s.interval_p95,
            s.interval_max,
            s.build_p95,
            s.build_max,
            stats.worst_interval,
            stats.long_frames,
            stats.blank_rows,
            self.meta_avg_us(),
            first_paint,
        );
        div()
            .h(px(TOOLBAR_H))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(16.))
            .px(px(16.))
            .bg(rgb(color::BG_PANEL))
            .border_b_1()
            .border_color(rgb(color::LINE))
            .child(
                div()
                    .text_color(rgb(color::INK))
                    .child(self.repo_path.clone()),
            )
            .child(div().text_color(rgb(color::INK_MUTED)).child(loaded))
            .child(div().flex_1())
            .child(
                div()
                    .id("readout")
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(color::INK_MUTED))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stats.borrow_mut().reset(Instant::now());
                        (this.meta_reads, this.meta_time) = (0, Duration::ZERO);
                        println!("spike-graph: stats reset");
                        cx.notify();
                    }))
                    .child(readout),
            )
    }

    fn render_scrollbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let track = self.track.clone();
        let thumb_color = if self.drag.is_some() {
            color::INK_FAINT
        } else {
            color::LINE_CONTROL
        };
        div()
            .id("scrollbar")
            .flex_none()
            .w(px(SCROLLBAR_W))
            .h_full()
            .relative()
            .bg(rgb(color::BG_PANEL))
            .child(
                canvas(move |bounds, _, _| track.set(bounds), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .children(self.thumb().map(|thumb| {
                div()
                    .absolute()
                    .top(px(thumb.top))
                    .left(px(2.))
                    .w(px(SCROLLBAR_W - 4.))
                    .h(px(thumb.height))
                    .rounded(px(4.))
                    .bg(rgb(thumb_color))
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.start_drag(event.position.y, cx)
                }),
            )
    }
}

fn lane_color(color: u8) -> Hsla {
    rgb(color::LANES[color as usize % color::LANES.len()]).into()
}

#[allow(clippy::too_many_arguments)]
fn render_row(
    head: bool,
    row: &Row,
    edges: Vec<Edge>,
    passing: Vec<(u16, u8)>,
    graph_w: f32,
    meta: &Meta,
    hash: String,
    overflow: u16,
) -> Div {
    let dot = Dot {
        column: row.column,
        color: row.color,
        merge: row.merge,
        head,
    };
    let graph = canvas(
        |_, _, _| (),
        move |bounds, _, window, _| paint_lanes(window, bounds, dot, &edges, &passing),
    )
    .flex_none()
    .w(px(graph_w))
    .h_full();
    div()
        .h(px(ROW_H))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .bg(rgb(color::BG_PANEL))
        .child(graph)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .pr(px(16.))
                .text_color(rgb(color::INK))
                .when(overflow > 0, |d| {
                    d.child(
                        div()
                            .flex_none()
                            .text_color(rgb(color::INK_FAINT))
                            .child(format!("+{overflow} ")),
                    )
                    .flex()
                    .flex_row()
                })
                .child(meta.subject.clone()),
        )
        .child(
            div()
                .flex_none()
                .w(px(180.))
                .pr(px(16.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(rgb(color::INK_MUTED))
                .child(meta.author.clone()),
        )
        .child(
            div()
                .flex_none()
                .w(px(96.))
                .text_color(rgb(color::INK_MUTED))
                .child(meta.date.clone()),
        )
        .child(
            div()
                .flex_none()
                .w(px(88.))
                .font_family(CODE_FONT)
                .text_size(px(12.))
                .text_color(rgb(color::INK_FAINT))
                .child(hash),
        )
}

#[derive(Clone, Copy)]
struct Dot {
    column: u16,
    color: u8,
    merge: bool,
    head: bool,
}

fn paint_lanes(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    dot: Dot,
    edges: &[Edge],
    passing: &[(u16, u8)],
) {
    let Dot {
        column,
        color: dot_color,
        merge,
        head,
    } = dot;
    let x = |col: u16| bounds.origin.x + px(GRAPH_PAD + f32::from(col) * LANE_W + LANE_W / 2.0);
    let top = bounds.origin.y;
    let mid = top + px(ROW_H / 2.0);
    let bottom = top + px(ROW_H);
    let vline = |window: &mut Window, col: u16, from: Pixels, to: Pixels, color: u8| {
        let origin = point(x(col) - px(STROKE / 2.0), from);
        window.paint_quad(fill(
            Bounds::new(origin, size(px(STROKE), to - from)),
            lane_color(color),
        ));
    };
    // A quarter curve between two columns, for edges that change lane.
    let curve = |window: &mut Window, from: Point<Pixels>, to: Point<Pixels>, color: u8| {
        let mut path = PathBuilder::stroke(px(STROKE));
        path.move_to(from);
        path.curve_to(to, point(from.x, to.y));
        if let Ok(path) = path.build() {
            window.paint_path(path, lane_color(color));
        }
    };
    let drawn = |col: u16| col < MAX_LANES;
    for &(col, color) in passing {
        vline(window, col, top, bottom, color);
    }
    for edge in edges {
        match *edge {
            Edge::In { from, color } if drawn(from) => {
                if from == column {
                    vline(window, column, top, mid, color);
                } else if drawn(column) {
                    curve(window, point(x(from), top), point(x(column), mid), color);
                }
            }
            Edge::Out { to, color } if drawn(to) => {
                if to == column {
                    vline(window, column, mid, bottom, color);
                } else if drawn(column) {
                    curve(window, point(x(column), mid), point(x(to), bottom), color);
                }
            }
            _ => {}
        }
    }
    if !drawn(column) {
        return;
    }
    let dot = Bounds::new(
        point(x(column) - px(DOT_R), mid - px(DOT_R)),
        size(px(DOT_R * 2.0), px(DOT_R * 2.0)),
    );
    let (background, border, border_color): (Hsla, f32, Hsla) = if head {
        (rgb(color::BG_PANEL).into(), 2.0, rgb(color::ACCENT).into())
    } else if merge {
        (rgb(color::BG_PANEL).into(), STROKE, lane_color(dot_color))
    } else {
        (lane_color(dot_color), 0.0, lane_color(dot_color))
    };
    window.paint_quad(quad(
        dot,
        Corners::all(px(DOT_R)),
        background,
        Edges::all(px(border)),
        border_color,
        BorderStyle::Solid,
    ));
}

impl Render for GraphView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        self.stats.borrow_mut().render_started(now);
        window.request_animation_frame();
        self.drain_events();
        self.step_sweep(now, cx);

        let rows = self.rows.len();
        let visible = self.visible_rows();
        self.drain_prefetch();
        self.request_prefetch(visible.clone());
        let graph_w = self.graph_width(visible);

        let body = match &self.phase {
            Phase::Loading => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(color::INK_FAINT))
                .child("Walking commits…"),
            Phase::Failed(error) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(color::REMOVE_INK))
                .child(error.clone()),
            Phase::Ready => div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_row()
                .child(
                    uniform_list(
                        "graph-rows",
                        rows,
                        cx.processor(move |this, range, _window, _cx| {
                            this.render_rows(range, graph_w)
                        }),
                    )
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .h_full(),
                )
                .child(self.render_scrollbar(cx)),
        };

        let stats = self.stats.clone();
        let first_page_ready = matches!(self.phase, Phase::Ready)
            .then_some(self.first_page_ready)
            .flatten();
        let frame_probe = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let now = Instant::now();
                let mut stats = stats.borrow_mut();
                if let (Some(ready), None) = (first_page_ready, stats.first_paint) {
                    let first = now - ready;
                    stats.first_paint = Some(first);
                    println!(
                        "spike-graph: first paint {:.1} ms after the first page was ready",
                        first.as_secs_f32() * 1000.0
                    );
                    window.on_next_frame(move |_, _| {
                        println!(
                            "spike-graph: next frame began {:.1} ms after the first page was ready",
                            ready.elapsed().as_secs_f32() * 1000.0
                        );
                    });
                }
                if let Some(s) = stats.painted(now) {
                    println!(
                        "spike-graph: {} frames, {:.1} fps · interval p95 {:.1} max {:.1} ms · build p95 {:.1} max {:.1} ms · since reset: worst {:.1} ms, >50 ms {}, blank {}",
                        s.frames,
                        s.fps,
                        s.interval_p95,
                        s.interval_max,
                        s.build_p95,
                        s.build_max,
                        stats.worst_interval,
                        stats.long_frames,
                        stats.blank_rows,
                    );
                }
            },
        )
        .absolute()
        .size_full();

        let drag_listeners = self.drag.is_some().then(|| {
            let (on_move, on_up) = (cx.entity(), cx.entity());
            canvas(
                |_, _, _| (),
                move |_, _, window, _| {
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                        if phase != DispatchPhase::Bubble {
                            return;
                        }
                        on_move.update(cx, |view, cx| {
                            if event.dragging() {
                                view.drag_to(event.position.y, cx);
                            } else {
                                view.drag = None;
                                cx.notify();
                            }
                        });
                    });
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                            on_up.update(cx, |view, cx| {
                                view.drag = None;
                                cx.notify();
                            });
                        }
                    });
                },
            )
            .absolute()
            .size_full()
        });

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(rgb(color::BG_APP))
            .font_family(UI_FONT)
            .text_size(px(13.))
            .line_height(px(ROW_H))
            .text_color(rgb(color::INK))
            .child(self.render_header(cx))
            .child(body)
            .children(drag_listeners)
            .child(frame_probe)
    }
}
