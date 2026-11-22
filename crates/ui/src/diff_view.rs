//! The diff viewer (backlog task 10): split and unified layouts of one
//! file's diff in a virtualized list, with line numbers, add and remove
//! colours, syntax colours and word-level change marks.
//!
//! It only renders what it is given (`Content`) and sends nothing back:
//! computing the diff and the highlight is the caller's job, on a
//! background thread. Rows are a fixed `row-diff` tall, so the list is a
//! `uniform_list`, and every row is built from data already in memory: the
//! syntax spans cover the whole file, so scrolling or dragging the
//! scrollbar never waits for a highlight.

use std::sync::Arc;

use diff::{DiffLine, DiffNote, Eol, LineKind, TextDiff};
use gpui::{
    AnyElement, Context, Div, FontWeight, HighlightStyle, IntoElement, ParentElement, Render,
    SharedString, Styled, StyledText, UniformListScrollHandle, Window, div, point, px,
    uniform_list,
};
use highlight::{Capture, Span};
use tokens::{
    ROW_DIFF, SPACE_2, SPACE_3, SPACE_4, TEXT_BODY, TEXT_BODY_MEDIUM, TEXT_CODE, TEXT_CODE_SMALL,
    TEXT_SMALL, TOOLBAR, Theme,
};

use crate::code_text::{expand_tabs, expanded_offset, merge_styles};
use crate::components::{Banner, BannerKind, Segmented};
use crate::convert::hsla;
use crate::diff_rows::{Layout, Row, hunk_header, rows};
use crate::scrollbar::Scrollbar;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// Width of the slot where line-staging checkboxes will go (task 11).
const SELECT_SLOT: f32 = 24.0;
/// Width of a line number column.
const NUMBER_W: f32 = 36.0;
/// Width of one digit of the code font (JetBrains Mono: 0.6 em at 13 px),
/// rounded up.
const DIGIT_W: f32 = 8.0;
/// Width of the `+` / `−` column.
const SIGN_W: f32 = 14.0;
const NO_NEWLINE: &str = "  \\ No newline at end of file";

/// What the viewer shows.
pub enum Content {
    /// Waiting for the diff to be computed.
    Loading,
    /// A message instead of a diff: no file selected, binary, not text.
    Message(SharedString),
    Text(Box<DiffData>),
}

/// One file's diff, with the syntax spans of both sides.
pub struct DiffData {
    pub path: SharedString,
    pub diff: TextDiff,
    /// Spans over `diff.old` and `diff.new`; empty for plain text.
    pub old_spans: Arc<[Span]>,
    pub new_spans: Arc<[Span]>,
}

/// How many unchanged lines surround each change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Around {
    #[default]
    Three,
    Ten,
    WholeFile,
}

impl Around {
    const ALL: [Around; 3] = [Around::Three, Around::Ten, Around::WholeFile];

    fn lines(self) -> u32 {
        match self {
            Around::Three => 3,
            Around::Ten => 10,
            Around::WholeFile => u32::MAX,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Around::Three => "3 lines",
            Around::Ten => "10 lines",
            Around::WholeFile => "Whole file",
        }
    }
}

pub struct DiffView {
    content: Content,
    layout: Layout,
    around: Around,
    rows: Vec<Row>,
    /// Width of a line number column: as many digits as the longest number.
    number_w: f32,
    scroll: UniformListScrollHandle,
}

impl Default for DiffView {
    fn default() -> Self {
        Self::new()
    }
}

impl DiffView {
    pub fn new() -> Self {
        Self {
            content: Content::Message("No file selected".into()),
            layout: Layout::default(),
            around: Around::default(),
            rows: Vec::new(),
            number_w: NUMBER_W,
            scroll: UniformListScrollHandle::new(),
        }
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn set_content(&mut self, content: Content, cx: &mut Context<Self>) {
        self.content = content;
        self.rebuild();
        self.scroll_to_top();
        cx.notify();
    }

    pub fn set_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        if self.layout != layout {
            self.layout = layout;
            self.rebuild();
            self.scroll_to_top();
            cx.notify();
        }
    }

    pub fn set_around(&mut self, around: Around, cx: &mut Context<Self>) {
        if self.around != around {
            self.around = around;
            self.rebuild();
            cx.notify();
        }
    }

    fn scroll_to_top(&self) {
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(px(0.), px(0.)));
    }

    /// Recomputes the hunks for the chosen context and the rows for the layout.
    fn rebuild(&mut self) {
        let Content::Text(data) = &mut self.content else {
            self.rows.clear();
            return;
        };
        if data.diff.note.is_none() {
            data.diff.hunks = data.diff.hunks_with_context(self.around.lines());
        }
        self.rows = rows(&data.diff, self.layout);
        self.number_w = number_width(&data.diff);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let mut bar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(SPACE_3))
            .h(px(TOOLBAR))
            .w_full()
            .px(px(SPACE_4))
            .border_b_1()
            .border_color(hsla(p.line));
        if let Content::Text(data) = &self.content {
            let stats = data.diff.stats;
            bar = bar
                .child(
                    div()
                        .text_style(&TEXT_BODY_MEDIUM, cx)
                        .text_color(hsla(p.ink))
                        .truncate()
                        .child(data.path.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.add_ink))
                        .child(format!("+{}", stats.added)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.remove_ink))
                        .child(format!("−{}", stats.removed)),
                );
        }
        let (layout_view, around_view) = (cx.entity(), cx.entity());
        bar.child(div().flex_1())
            .child(Segmented::new(
                "diff-around",
                Around::ALL.iter().map(|a| a.label().into()).collect(),
                Around::ALL
                    .iter()
                    .position(|a| *a == self.around)
                    .unwrap_or(0),
                move |index, _, cx| {
                    around_view.update(cx, |view, cx| view.set_around(Around::ALL[index], cx))
                },
            ))
            .child(Segmented::new(
                "diff-layout",
                vec!["Split".into(), "Unified".into()],
                match self.layout {
                    Layout::Split => 0,
                    Layout::Unified => 1,
                },
                move |index, _, cx| {
                    layout_view.update(cx, |view, cx| {
                        view.set_layout(
                            if index == 0 {
                                Layout::Split
                            } else {
                                Layout::Unified
                            },
                            cx,
                        )
                    })
                },
            ))
    }

    fn message(&self, text: SharedString, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .text_style(&TEXT_BODY, cx)
            .text_color(hsla(cx.palette().ink_muted))
            .child(text)
            .into_any_element()
    }

    /// The rows `range` of the list. Word-level marks are computed here, for
    /// the hunks that scroll into view only.
    fn render_rows(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) -> Vec<Div> {
        let Content::Text(data) = &mut self.content else {
            return range.map(|_| div().h(px(ROW_DIFF))).collect();
        };
        for index in range.clone() {
            match self.rows.get(index) {
                Some(Row::Unified { hunk, .. }) | Some(Row::Split { hunk, .. }) => {
                    data.diff.fill_inline(*hunk);
                }
                _ => {}
            }
        }
        let theme = cx.ui_theme().theme();
        let p = cx.palette();
        let (data, cx): (&DiffData, &gpui::App) = (data, cx);
        let ctx = Ctx {
            data,
            number_w: self.number_w,
            theme,
            p,
            cx,
        };
        range
            .map(|index| match self.rows.get(index) {
                Some(Row::HunkHeader { hunk }) => hunk_row(&ctx, *hunk),
                Some(Row::Unified { hunk, line }) => unified_row(&ctx, *hunk, *line),
                Some(Row::Split { hunk, left, right }) => split_row(&ctx, *hunk, *left, *right),
                None => div().h(px(ROW_DIFF)),
            })
            .collect()
    }
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let body: AnyElement = match &self.content {
            Content::Loading => self.message("Reading the diff…".into(), cx),
            Content::Message(text) => self.message(text.clone(), cx),
            Content::Text(data) if data.diff.is_empty() => {
                self.message("No differences".into(), cx)
            }
            Content::Text(data) => {
                let note = (data.diff.note == Some(DiffNote::TooExpensive)).then(|| {
                    Banner::new(
                        BannerKind::Warning,
                        "This diff was too expensive to compute, so the whole file is shown as replaced.",
                    )
                });
                let list = uniform_list(
                    "diff-rows",
                    self.rows.len(),
                    cx.processor(|this, range, _window, cx| this.render_rows(range, cx)),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .h_full();
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .children(note)
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h(px(0.))
                            .child(list)
                            .child(Scrollbar::new(
                                "diff-scrollbar",
                                self.scroll.clone(),
                                self.rows.len(),
                                ROW_DIFF,
                            )),
                    )
                    .into_any_element()
            }
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(hsla(p.bg_panel))
            .child(self.render_toolbar(cx))
            .child(body)
    }
}

/// What drawing a row needs, gathered once per frame.
struct Ctx<'a> {
    data: &'a DiffData,
    number_w: f32,
    theme: Theme,
    p: &'a tokens::Palette,
    cx: &'a gpui::App,
}

fn hunk_row(ctx: &Ctx, hunk: usize) -> Div {
    let (data, p, cx) = (ctx.data, ctx.p, ctx.cx);
    div()
        .flex()
        .items_center()
        .h(px(ROW_DIFF))
        .w_full()
        .px(px(SPACE_3))
        .bg(hsla(p.bg_raised))
        .text_style(&TEXT_CODE_SMALL, cx)
        .text_color(hsla(p.ink_muted))
        .child(hunk_header(&data.diff.hunks[hunk]))
}

/// The width of the line number columns: room for the longest number the
/// hunks show, at least three digits.
fn number_width(diff: &TextDiff) -> f32 {
    let longest = diff
        .hunks
        .iter()
        .map(|h| (h.old_start + h.old_len).max(h.new_start + h.new_len))
        .max()
        .unwrap_or(0);
    let digits = longest.to_string().len().max(3);
    digits as f32 * DIGIT_W + SPACE_2
}

/// The fill behind a line of `kind`.
fn fill(kind: LineKind, p: &tokens::Palette) -> tokens::Rgba {
    match kind {
        LineKind::Added => p.add_bg,
        LineKind::Removed => p.remove_bg,
        LineKind::Context => p.bg_panel,
    }
}

fn sign(kind: LineKind) -> &'static str {
    match kind {
        LineKind::Added => "+",
        LineKind::Removed => "−",
        LineKind::Context => "",
    }
}

fn sign_color(kind: LineKind, p: &tokens::Palette) -> tokens::Rgba {
    match kind {
        LineKind::Added => p.add_ink,
        LineKind::Removed => p.remove_ink,
        LineKind::Context => p.ink_faint,
    }
}

fn number_cell(number: Option<u32>, width: f32, p: &tokens::Palette) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .whitespace_nowrap()
        .pr(px(SPACE_2))
        .text_right()
        .text_color(hsla(p.ink_faint))
        .child(number.map(|n| n.to_string()).unwrap_or_default())
}

fn sign_cell(kind: LineKind, p: &tokens::Palette) -> Div {
    div()
        .flex_none()
        .w(px(SIGN_W))
        .text_color(hsla(sign_color(kind, p)))
        .child(sign(kind))
}

fn base_row(cx: &gpui::App) -> Div {
    div()
        .flex()
        .h(px(ROW_DIFF))
        .w_full()
        .text_style(&TEXT_CODE, cx)
}

fn unified_row(ctx: &Ctx, hunk: usize, line: usize) -> Div {
    let (data, p, nw) = (ctx.data, ctx.p, ctx.number_w);
    let l = &data.diff.hunks[hunk].lines[line];
    base_row(ctx.cx)
        .bg(hsla(fill(l.kind, p)))
        .child(div().flex_none().w(px(SELECT_SLOT)))
        .child(number_cell(l.old_no, nw, p))
        .child(number_cell(l.new_no, nw, p))
        .child(sign_cell(l.kind, p))
        .child(code_cell(ctx, l))
}

fn split_row(ctx: &Ctx, hunk: usize, left: Option<usize>, right: Option<usize>) -> Div {
    let (data, p, nw) = (ctx.data, ctx.p, ctx.number_w);
    let lines = &data.diff.hunks[hunk].lines;
    let half = |index: Option<usize>, left_side: bool| -> Div {
        let Some(index) = index else {
            // Nothing on this side: a blank cell.
            return div().flex_1().min_w(px(0.)).h_full().bg(hsla(p.bg_app));
        };
        let l = &lines[index];
        let number = if left_side { l.old_no } else { l.new_no };
        div()
            .flex()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .bg(hsla(fill(l.kind, p)))
            .child(div().flex_none().w(px(SELECT_SLOT)))
            .child(number_cell(number, nw, p))
            .child(sign_cell(l.kind, p))
            .child(code_cell(ctx, l))
    };
    base_row(ctx.cx)
        .child(half(left, true))
        .child(div().flex_none().w(px(1.)).h_full().bg(hsla(p.line)))
        .child(half(right, false))
}

/// The text of one line, coloured and with its word-level changes marked.
fn code_cell(ctx: &Ctx, line: &DiffLine) -> Div {
    let (data, theme, p) = (ctx.data, ctx.theme, ctx.p);
    let raw = data.diff.line_text(line);
    let text = expand_tabs(raw);
    let has_tabs = text.len() != raw.len();
    let shift = |offset: usize| {
        if has_tabs {
            expanded_offset(raw, offset)
        } else {
            offset
        }
    };

    let spans = match line.kind {
        LineKind::Removed => &data.old_spans,
        _ => &data.new_spans,
    };
    let absolute = line.text.as_range();
    let syntax: Vec<_> = highlight::spans_in(spans, absolute.clone())
        .iter()
        .filter_map(|span| {
            let start = span.start.max(absolute.start) - absolute.start;
            let end = span.end.min(absolute.end) - absolute.start;
            (start < end).then(|| (shift(start)..shift(end), span.capture))
        })
        .collect();
    let changed: Vec<_> = line
        .inline
        .as_ref()
        .map(|inline| {
            inline
                .0
                .iter()
                .map(|range| shift(range.start as usize)..shift(range.end as usize))
                .collect()
        })
        .unwrap_or_default();

    let strong = match line.kind {
        LineKind::Added => Some(hsla(p.add_bg_strong)),
        LineKind::Removed => Some(hsla(p.remove_bg_strong)),
        LineKind::Context => None,
    };
    let mut highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> =
        merge_styles(text.len(), &syntax, &changed)
            .into_iter()
            .map(|(range, style)| {
                let style = HighlightStyle {
                    color: style.capture.map(|c: Capture| hsla(c.token(theme))),
                    background_color: if style.changed { strong } else { None },
                    ..Default::default()
                };
                (range, style)
            })
            .collect();

    let mut text = text;
    if line.eol == Eol::None {
        let start = text.len();
        text.push_str(NO_NEWLINE);
        highlights.push((
            start..text.len(),
            HighlightStyle {
                color: Some(hsla(p.ink_faint)),
                font_weight: Some(FontWeight(400.0)),
                ..Default::default()
            },
        ));
    }

    div()
        .flex_1()
        .min_w(px(0.))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_color(hsla(p.ink))
        .child(StyledText::new(text).with_highlights(highlights))
}

/// Diffs two texts for the viewer, with no syntax colours yet. CPU only:
/// call it off the UI thread (a large file takes tens of milliseconds).
pub fn compute(path: &str, old: &str, new: &str) -> DiffData {
    DiffData {
        path: path.to_owned().into(),
        diff: diff::diff_text(old, new, &diff::DiffOptions::default()),
        old_spans: Arc::from(Vec::new()),
        new_spans: Arc::from(Vec::new()),
    }
}

/// The syntax spans of both sides (`old` and `new` are `diff.old` and
/// `diff.new`), parsed whole so highlighting is
/// right across hunk boundaries. Plain text (no grammar for `path`, a file
/// over the size limit) gives empty lists. CPU only, and slow on a huge
/// file (about 0.7 s for 1.5 MB of Rust): call it off the UI thread and
/// deliver the result with [`DiffView::set_spans`]. The two sides are parsed
/// at the same time.
pub fn highlight_sides(path: &str, old: &str, new: &str) -> (Arc<[Span]>, Arc<[Span]>) {
    let first_line = new.lines().next().or_else(|| old.lines().next());
    let Some(language) = highlight::detect(path, None, first_line) else {
        return (Arc::from(Vec::new()), Arc::from(Vec::new()));
    };
    let parse = |text: &str| -> Arc<[Span]> {
        highlight::highlight(language, text, &|| false)
            .map(Arc::from)
            .unwrap_or_else(|_| Arc::from(Vec::new()))
    };
    std::thread::scope(|scope| {
        let old_spans = scope.spawn(|| parse(old));
        let new_spans = parse(new);
        (
            old_spans.join().unwrap_or_else(|_| Arc::from(Vec::new())),
            new_spans,
        )
    })
}

impl DiffView {
    /// Adds the syntax colours once they are ready. The spans must be those
    /// of the diff currently shown (see [`highlight_sides`]); the caller
    /// drops results for a file that is no longer selected.
    pub fn set_spans(&mut self, old: Arc<[Span]>, new: Arc<[Span]>, cx: &mut Context<Self>) {
        if let Content::Text(data) = &mut self.content {
            data.old_spans = old;
            data.new_spans = new;
            cx.notify();
        }
    }
}

impl DiffView {
    /// Shows new content for the same file (a refresh): the scroll position
    /// is kept, where [`DiffView::set_content`] goes back to the top.
    pub fn replace_content(&mut self, content: Content, cx: &mut Context<Self>) {
        self.content = content;
        self.rebuild();
        cx.notify();
    }
}
