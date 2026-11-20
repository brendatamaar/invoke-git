//! The split diff view: a virtualized list, a custom scrollbar, background
//! highlighting and the measurement readout.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    AppContext, Bounds, Context, DispatchPhase, Div, HighlightStyle, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, SharedString, StyledText, UniformListScrollHandle,
    Window, canvas, div, point, prelude::*, px, rgb, uniform_list,
};
use tree_sitter::Tree;

use crate::highlight::{BUFFER, CHUNK, ChunkSpans, Highlighter, Token};
use crate::model::{self, Loaded, Row, SideText};
use crate::stats::FrameStats;

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
    pub const ADD_BG: u32 = 0x132a1d;
    pub const ADD_INK: u32 = 0x6fdb9c;
    pub const REMOVE_BG: u32 = 0x2e1719;
    pub const REMOVE_INK: u32 = 0xff9196;
    pub const WARNING: u32 = 0xf2b350;
    pub const LANE_2: u32 = 0x3fc7b5;
    pub const LANE_7: u32 = 0x62b8f5;
}

/// `row-diff` and `code` line height.
const ROW_H: f32 = 20.0;
const TOOLBAR_H: f32 = 40.0;
const SCROLLBAR_W: f32 = 12.0;
const MIN_THUMB: f32 = 24.0;
/// Windows fallbacks named in docs/design/README.md.
const CODE_FONT: &str = "Cascadia Mono";
const UI_FONT: &str = "Segoe UI";

fn token_color(token: Token) -> u32 {
    match token {
        Token::Keyword => color::ACCENT,
        Token::String => color::ADD_INK,
        Token::Comment => color::INK_FAINT,
        Token::Type => color::LANE_2,
        Token::Function => color::LANE_7,
        Token::Number => color::WARNING,
    }
}

struct SideHighlight {
    text: Arc<SideText>,
    tree: Option<Tree>,
    chunks: HashMap<usize, ChunkSpans>,
    pending: HashSet<usize>,
}

struct Ready {
    data: Loaded,
    sides: [SideHighlight; 2],
}

enum Phase {
    Loading,
    Failed(SharedString),
    Ready(Box<Ready>),
}

#[derive(Clone, Copy)]
enum CellKind {
    Context,
    Removed,
    Added,
    Filler,
}

struct Thumb {
    top: f32,
    height: f32,
    track: f32,
    max_scroll: f32,
}

/// Automated scroll for repeatable measurement: `--sweep SECONDS` scrolls top
/// to bottom at constant speed once highlighting is ready, then quits.
#[derive(Clone, Copy)]
pub enum Sweep {
    Off,
    Waiting { seconds: f32 },
    Running { start: Instant, seconds: f32 },
    Done,
}

pub struct DiffView {
    title: SharedString,
    phase: Phase,
    highlighter: Arc<Highlighter>,
    scroll: UniformListScrollHandle,
    stats: Rc<RefCell<FrameStats>>,
    /// Scrollbar track bounds from the last paint, in window coordinates.
    track: Rc<Cell<Bounds<Pixels>>>,
    /// While dragging the thumb: where inside the thumb it was grabbed.
    drag: Option<f32>,
    sweep: Sweep,
}

impl DiffView {
    pub fn new(old: PathBuf, new: PathBuf, sweep: Sweep, cx: &mut Context<Self>) -> Self {
        let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned());
        let title = format!(
            "{} → {}",
            name(&old).unwrap_or_default(),
            name(&new).unwrap_or_default()
        );
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { model::load(&old, &new) })
                .await;
            this.update(cx, |view, cx| view.loaded(result, cx)).ok();
        })
        .detach();
        Self {
            title: title.into(),
            phase: Phase::Loading,
            highlighter: Arc::new(Highlighter::new()),
            scroll: UniformListScrollHandle::new(),
            stats: Rc::new(RefCell::new(FrameStats::new(Instant::now()))),
            track: Rc::new(Cell::new(Bounds::default())),
            drag: None,
            sweep,
        }
    }

    fn loaded(&mut self, result: Result<Loaded, String>, cx: &mut Context<Self>) {
        let data = match result {
            Ok(data) => data,
            Err(error) => {
                eprintln!("spike-diff: {error}. Run `cargo xtask fixtures` first.");
                self.phase = Phase::Failed(error.into());
                cx.notify();
                return;
            }
        };
        let ms = |d: std::time::Duration| d.as_secs_f32() * 1000.0;
        println!(
            "spike-diff: read {:.1} ms | after data ready: index {:.1} ms, diff {:.1} ms, rows {:.1} ms, to UI thread {:.1} ms | {} old lines, {} new lines, {} rows, +{} -{}",
            ms(data.read_time),
            ms(data.index_time),
            ms(data.diff_time),
            ms(data.rows_time),
            ms(data.data_ready.elapsed()),
            data.old.lines.len(),
            data.new.lines.len(),
            data.rows.len(),
            data.added,
            data.removed,
        );
        let side = |text: &Arc<SideText>| SideHighlight {
            text: text.clone(),
            tree: None,
            chunks: HashMap::new(),
            pending: HashSet::new(),
        };
        let sides = [side(&data.old), side(&data.new)];
        for (index, side) in sides.iter().enumerate() {
            let text = side.text.clone();
            cx.spawn(async move |this, cx| {
                let (tree, took) = cx
                    .background_spawn(async move {
                        let started = Instant::now();
                        (Highlighter::parse(&text.text), started.elapsed())
                    })
                    .await;
                let name = ["old", "new"][index];
                println!(
                    "spike-diff: parsed {name} side in {:.1} ms",
                    took.as_secs_f32() * 1000.0
                );
                this.update(cx, |view, cx| {
                    if let Phase::Ready(ready) = &mut view.phase {
                        ready.sides[index].tree = tree;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        self.phase = Phase::Ready(Box::new(Ready { data, sides }));
        cx.notify();
    }

    /// Starts background highlight jobs for chunks around `lines` on one side.
    fn request_highlights(&mut self, index: usize, lines: Range<usize>, cx: &mut Context<Self>) {
        let highlighter = self.highlighter.clone();
        let Phase::Ready(ready) = &mut self.phase else {
            return;
        };
        let side = &mut ready.sides[index];
        let Some(tree) = side.tree.clone() else {
            return;
        };
        let len = side.text.lines.len();
        let first = lines.start.saturating_sub(BUFFER) / CHUNK;
        let last = (lines.end + BUFFER).min(len).div_ceil(CHUNK);
        for chunk in first..last {
            if side.chunks.contains_key(&chunk) || !side.pending.insert(chunk) {
                continue;
            }
            let (tree, text, highlighter) = (tree.clone(), side.text.clone(), highlighter.clone());
            cx.spawn(async move |this, cx| {
                let spans = cx
                    .background_spawn(async move { highlighter.style_chunk(&tree, &text, chunk) })
                    .await;
                this.update(cx, |view, cx| {
                    if let Phase::Ready(ready) = &mut view.phase {
                        let side = &mut ready.sides[index];
                        side.pending.remove(&chunk);
                        side.chunks.insert(chunk, Arc::new(spans));
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    fn scroll_top(&self) -> f32 {
        -f32::from(self.scroll.0.borrow().base_handle.offset().y)
    }

    fn visible_rows(&self, rows: usize) -> Range<usize> {
        let height = f32::from(self.track.get().size.height).max(ROW_H);
        let first = (self.scroll_top() / ROW_H).floor().max(0.0) as usize;
        let count = (height / ROW_H).ceil() as usize + 1;
        first.min(rows)..(first + count).min(rows)
    }

    fn thumb(&self, rows: usize) -> Option<Thumb> {
        let track = f32::from(self.track.get().size.height);
        let content = rows as f32 * ROW_H;
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

    /// Advances the automated sweep; called at the start of each render.
    fn step_sweep(&mut self, now: Instant, cx: &mut Context<Self>) {
        let parsed = match &self.phase {
            Phase::Ready(ready) => ready.sides.iter().all(|side| side.tree.is_some()),
            _ => false,
        };
        let content = self.row_count() as f32 * ROW_H;
        let max_scroll = (content - f32::from(self.track.get().size.height)).max(0.0);
        match self.sweep {
            Sweep::Waiting { seconds } if parsed && max_scroll > 0.0 => {
                self.stats.borrow_mut().reset(now);
                println!("spike-diff: sweep start, {seconds} s top to bottom");
                self.sweep = Sweep::Running {
                    start: now,
                    seconds,
                };
            }
            Sweep::Running { start, seconds } => {
                let t = ((now - start).as_secs_f32() / seconds).min(1.0);
                self.scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(px(0.), px(-t * max_scroll)));
                if t >= 1.0 {
                    let stats = self.stats.borrow();
                    println!(
                        "spike-diff: sweep done · worst frame {:.1} ms · >50 ms {} · blank {} · unstyled {}",
                        stats.worst_interval,
                        stats.long_frames,
                        stats.blank_rows,
                        stats.unstyled_rows
                    );
                    self.sweep = Sweep::Done;
                    cx.quit();
                }
            }
            _ => {}
        }
    }

    fn row_count(&self) -> usize {
        match &self.phase {
            Phase::Ready(ready) => ready.data.rows.len(),
            _ => 0,
        }
    }

    fn start_drag(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let Some(thumb) = self.thumb(self.row_count()) else {
            return;
        };
        let local = f32::from(y - self.track.get().origin.y);
        let on_thumb = (thumb.top..thumb.top + thumb.height).contains(&local);
        // Clicking the track jumps there, centred on the pointer.
        self.drag = Some(if on_thumb {
            local - thumb.top
        } else {
            thumb.height / 2.0
        });
        self.drag_to(y, cx);
    }

    fn drag_to(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let (Some(grab), Some(thumb)) = (self.drag, self.thumb(self.row_count())) else {
            return;
        };
        let local = f32::from(y - self.track.get().origin.y);
        let room = thumb.track - thumb.height;
        let top = (local - grab).clamp(0.0, room);
        let scroll = top / room * thumb.max_scroll;
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(-scroll)));
        cx.notify();
    }

    fn render_rows(&mut self, range: Range<usize>, _cx: &mut Context<Self>) -> Vec<Div> {
        let mut stats = self.stats.borrow_mut();
        let Phase::Ready(ready) = &self.phase else {
            stats.blank_rows += range.len() as u64;
            return range.map(|_| div().h(px(ROW_H))).collect();
        };
        let mut rows = Vec::with_capacity(range.len());
        for ix in range {
            let Some(&row) = ready.data.rows.get(ix) else {
                stats.blank_rows += 1;
                rows.push(div().h(px(ROW_H)));
                continue;
            };
            let (left, right) = match row {
                Row::Context { old, new } => (
                    (Some(old), CellKind::Context),
                    (Some(new), CellKind::Context),
                ),
                Row::Change { old, new } => (
                    (old, old.map_or(CellKind::Filler, |_| CellKind::Removed)),
                    (new, new.map_or(CellKind::Filler, |_| CellKind::Added)),
                ),
            };
            let mut cell = |index: usize, (line, kind): (Option<u32>, CellKind)| {
                let side = &ready.sides[index];
                let text = line.map(|line| {
                    let line = line as usize;
                    match side.text.lines.get(line) {
                        Some(_) => {
                            styled_line(side.text.line(line), side, line, &mut stats.unstyled_rows)
                        }
                        None => {
                            stats.blank_rows += 1;
                            StyledText::new("")
                        }
                    }
                });
                render_cell(line, kind, text)
            };
            let left = cell(0, left);
            let right = cell(1, right);
            rows.push(
                div()
                    .h(px(ROW_H))
                    .w_full()
                    .flex()
                    .flex_row()
                    .child(left)
                    .child(div().flex_none().w(px(1.)).h_full().bg(rgb(color::LINE)))
                    .child(right),
            );
        }
        rows
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (added, removed) = match &self.phase {
            Phase::Ready(ready) => (ready.data.added, ready.data.removed),
            _ => (0, 0),
        };
        let stats = self.stats.borrow();
        let s = stats.last;
        let first_paint = stats
            .first_paint
            .map(|d| format!("{:.0} ms", d.as_secs_f32() * 1000.0))
            .unwrap_or_else(|| "—".into());
        let readout = format!(
            "{:.1} fps · frame p95 {:.1} max {:.1} ms · build p95 {:.1} max {:.1} ms · worst {:.1} ms · >50 ms {} · blank {} · unstyled {} · first paint {}",
            s.fps,
            s.interval_p95,
            s.interval_max,
            s.build_p95,
            s.build_max,
            stats.worst_interval,
            stats.long_frames,
            stats.blank_rows,
            stats.unstyled_rows,
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
            .font_family(UI_FONT)
            .text_size(px(13.))
            .child(div().text_color(rgb(color::INK)).child(self.title.clone()))
            .child(
                div()
                    .text_color(rgb(color::ADD_INK))
                    .child(format!("+{added}")),
            )
            .child(
                div()
                    .text_color(rgb(color::REMOVE_INK))
                    .child(format!("−{removed}")),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("readout")
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(color::INK_MUTED))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stats.borrow_mut().reset(Instant::now());
                        println!("spike-diff: stats reset");
                        cx.notify();
                    }))
                    .child(readout),
            )
    }

    fn render_scrollbar(&self, rows: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let track = self.track.clone();
        let thumb = self.thumb(rows);
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
            .children(thumb.map(|thumb| {
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

fn styled_line(text: &str, side: &SideHighlight, line: usize, unstyled: &mut u64) -> StyledText {
    let styled = StyledText::new(SharedString::from(text.to_string()));
    match side.chunks.get(&(line / CHUNK)) {
        Some(chunk) => styled.with_highlights(chunk[line % CHUNK].iter().map(|(range, token)| {
            let style = HighlightStyle {
                color: Some(rgb(token_color(*token)).into()),
                ..Default::default()
            };
            (range.clone(), style)
        })),
        None => {
            *unstyled += 1;
            styled
        }
    }
}

fn render_cell(line: Option<u32>, kind: CellKind, text: Option<StyledText>) -> Div {
    let (bg, sign, sign_color) = match kind {
        CellKind::Context => (color::BG_PANEL, None, color::INK_FAINT),
        CellKind::Removed => (color::REMOVE_BG, Some("−"), color::REMOVE_INK),
        CellKind::Added => (color::ADD_BG, Some("+"), color::ADD_INK),
        CellKind::Filler => (color::BG_APP, None, color::INK_FAINT),
    };
    div()
        .flex_1()
        .min_w(px(0.))
        .h_full()
        .flex()
        .flex_row()
        .overflow_hidden()
        .bg(rgb(bg))
        .child(
            div()
                .flex_none()
                .w(px(56.))
                .pr(px(8.))
                .text_right()
                .text_size(px(12.))
                .text_color(rgb(color::INK_FAINT))
                .children(line.map(|line| SharedString::from((line + 1).to_string()))),
        )
        .child(
            div()
                .flex_none()
                .w(px(16.))
                .text_color(rgb(sign_color))
                .children(sign),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(rgb(color::INK))
                .children(text),
        )
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        self.stats.borrow_mut().render_started(now);
        // Draw every frame so the frame-interval readout measures real pacing.
        window.request_animation_frame();
        self.step_sweep(now, cx);

        let rows = self.row_count();
        let data_ready = match &self.phase {
            Phase::Ready(ready) => Some(ready.data.data_ready),
            _ => None,
        };
        if let Phase::Ready(ready) = &self.phase {
            let visible = self.visible_rows(rows);
            let mut spans: [Option<Range<usize>>; 2] = [None, None];
            for row in &ready.data.rows[visible] {
                let (old, new) = match *row {
                    Row::Context { old, new } => (Some(old), Some(new)),
                    Row::Change { old, new } => (old, new),
                };
                for (slot, line) in spans.iter_mut().zip([old, new]) {
                    if let Some(line) = line.map(|l| l as usize) {
                        let range = slot.get_or_insert(line..line + 1);
                        range.start = range.start.min(line);
                        range.end = range.end.max(line + 1);
                    }
                }
            }
            for (index, lines) in spans.into_iter().enumerate() {
                if let Some(lines) = lines {
                    self.request_highlights(index, lines, cx);
                }
            }
        }

        let body = match &self.phase {
            Phase::Loading => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(color::INK_FAINT))
                .child("Loading fixtures…"),
            Phase::Failed(error) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(color::REMOVE_INK))
                .child(format!("{error}. Run `cargo xtask fixtures` first.")),
            Phase::Ready(_) => div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_row()
                .child(
                    uniform_list(
                        "diff-rows",
                        rows,
                        cx.processor(|this, range, _window, cx| this.render_rows(range, cx)),
                    )
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .h_full(),
                )
                .child(self.render_scrollbar(rows, cx)),
        };

        let stats = self.stats.clone();
        let frame_probe = canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let now = Instant::now();
                let mut stats = stats.borrow_mut();
                if let (Some(ready), None) = (data_ready, stats.first_paint) {
                    let first = now - ready;
                    stats.first_paint = Some(first);
                    println!(
                        "spike-diff: first paint {:.1} ms after data ready",
                        first.as_secs_f32() * 1000.0
                    );
                    window.on_next_frame(move |_, _| {
                        println!(
                            "spike-diff: next frame began {:.1} ms after data ready",
                            ready.elapsed().as_secs_f32() * 1000.0
                        );
                    });
                }
                if let Some(s) = stats.painted(now) {
                    println!(
                        "spike-diff: {} frames, {:.1} fps · interval p95 {:.1} max {:.1} ms · build p95 {:.1} max {:.1} ms · since reset: worst {:.1} ms, >50 ms {}, blank {}, unstyled {}",
                        s.frames,
                        s.fps,
                        s.interval_p95,
                        s.interval_max,
                        s.build_p95,
                        s.build_max,
                        stats.worst_interval,
                        stats.long_frames,
                        stats.blank_rows,
                        stats.unstyled_rows,
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
                                // Released outside the window.
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
            .font_family(CODE_FONT)
            .text_size(px(13.))
            .line_height(px(ROW_H))
            .text_color(rgb(color::INK))
            .child(self.render_header(cx))
            .child(body)
            .children(drag_listeners)
            .child(frame_probe)
    }
}
