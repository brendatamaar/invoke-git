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

use std::collections::BTreeSet;
use std::sync::Arc;

use diff::{DiffLine, DiffNote, Eol, LineKind, TextDiff};
use gpui::{
    AnyElement, App, Context, Div, ElementId, EventEmitter, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, ParentElement, PathBuilder, Render, Role, ScrollStrategy,
    SharedString, StatefulInteractiveElement, Styled, StyledText, Toggled, UniformListScrollHandle,
    Window, canvas, deferred, div, point, px, uniform_list,
};
use highlight::{Capture, Span};
use tokens::{
    RADIUS_LG, RADIUS_MD, RADIUS_SM, ROW_DIFF, ROW_LIST, SIDEBAR, SPACE_1, SPACE_2, SPACE_3,
    SPACE_4, SPACE_8, TEXT_BODY, TEXT_BODY_MEDIUM, TEXT_CODE, TEXT_CODE_SMALL, TEXT_SMALL, TOOLBAR,
    Theme,
};

use crate::code_text::{expand_tabs, expanded_offset, merge_styles};
use crate::components::{Banner, BannerKind, Button, Divider, Icon, IconName, Pill, Segmented};
use crate::convert::{hsla, shadows};
use crate::diff_rows::{Layout, Row, hunk_header, rows};
use crate::focus;
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
    FileMessage {
        text: SharedString,
        mode: StageMode,
    },
    Text(Box<DiffData>),
}

/// One file's diff, with the syntax spans of both sides.
/// What the diff can be used for: read only, or the source of a stage or an
/// unstage (the checkboxes and hunk buttons appear).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StageMode {
    #[default]
    ReadOnly,
    /// Index to worktree: selected changes can be staged.
    Stage,
    /// `HEAD` to index: selected changes can be unstaged.
    Unstage,
}

/// What a stage request needs to know about the sides the diff was made
/// from (see `shell::DiffInputs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputsMeta {
    pub based_on: invoke_core::IndexId,
    pub executable: bool,
    pub old_exists: bool,
    pub new_exists: bool,
    /// The git blob id of the worktree file's bytes (a discard expects it).
    pub new_blob: Option<invoke_core::Oid>,
}

/// A request from the diff: stage or unstage a part of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffEvent {
    /// Every change in the hunk (an index into the hunks as shown).
    Hunk { hunk: usize },
    /// The selected lines of one hunk.
    Lines { hunk: usize, lines: Vec<usize> },
    /// Throw away every unstaged change of the file (an unstaged diff only).
    DiscardFile,
    /// Throw away the unstaged changes of one hunk.
    DiscardHunk { hunk: usize },
    /// Throw away the selected unstaged lines of one hunk.
    DiscardLines { hunk: usize, lines: Vec<usize> },
    /// P10: request `git lfs pull` for the currently shown LFS pointer file.
    PullLfs,
    /// Open the Blame view for the currently shown file.
    Blame,
    /// Open the File History view for the currently shown file.
    FileHistory,
}

/// P10: Git LFS metadata shown as a badge and banner in the diff viewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LfsDiffInfo {
    /// Human-readable summary (e.g. `"Git LFS pointer · 12.1 KiB · sha256:4d7a214614ab"`).
    pub summary: SharedString,
    /// True when the file in the worktree is still an un-smudged pointer.
    pub is_pointer: bool,
}

pub struct DiffData {
    pub path: SharedString,
    pub mode: StageMode,
    pub inputs: Option<InputsMeta>,
    pub lfs: Option<LfsDiffInfo>,
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
    /// Selected changed lines as (hunk, line), for line staging.
    selection: BTreeSet<(usize, usize)>,
    scroll: UniformListScrollHandle,
    hunk_rows: Vec<usize>,
    navigated_hunk: usize,
    /// When true, show Blame and File history buttons in the toolbar when a file is loaded.
    show_file_insights: bool,
    context_menu_open: bool,
    actions_menu_open: bool,
}

impl EventEmitter<DiffEvent> for DiffView {}

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
            selection: BTreeSet::new(),
            scroll: UniformListScrollHandle::new(),
            hunk_rows: Vec::new(),
            navigated_hunk: 0,
            show_file_insights: false,
            context_menu_open: false,
            actions_menu_open: false,
        }
    }

    /// Shows compact Blame and File history triggers inside the diff toolbar.
    pub fn with_file_insights(mut self) -> Self {
        self.show_file_insights = true;
        self
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    pub fn set_content(&mut self, content: Content, cx: &mut Context<Self>) {
        self.content = content;
        self.selection.clear();
        self.context_menu_open = false;
        self.actions_menu_open = false;
        self.rebuild();
        self.scroll_to_top();
        cx.notify();
    }

    pub fn set_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        let had_menu = self.context_menu_open || self.actions_menu_open;
        self.context_menu_open = false;
        self.actions_menu_open = false;
        if self.layout != layout {
            self.layout = layout;
            let hunk = self.navigated_hunk;
            self.rebuild();
            self.show_hunk(hunk, cx);
            cx.notify();
        } else if had_menu {
            cx.notify();
        }
    }

    pub fn set_around(&mut self, around: Around, cx: &mut Context<Self>) {
        let had_menu = self.context_menu_open || self.actions_menu_open;
        self.context_menu_open = false;
        self.actions_menu_open = false;
        if self.around != around {
            self.around = around;
            self.selection.clear();
            self.rebuild();
            self.scroll_to_top();
            cx.notify();
        } else if had_menu {
            cx.notify();
        }
    }

    fn toggle_context_menu(&mut self, cx: &mut Context<Self>) {
        self.context_menu_open = !self.context_menu_open;
        self.actions_menu_open = false;
        cx.notify();
    }

    fn toggle_actions_menu(&mut self, cx: &mut Context<Self>) {
        self.actions_menu_open = !self.actions_menu_open;
        self.context_menu_open = false;
        cx.notify();
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
        self.navigated_hunk = 0;
        self.hunk_rows.clear();
        let Content::Text(data) = &mut self.content else {
            self.rows.clear();
            return;
        };
        if data.diff.note.is_none() {
            data.diff.hunks = data.diff.hunks_with_context(self.around.lines());
        }
        self.rows = rows(&data.diff, self.layout);
        self.number_w = number_width(&data.diff);
        self.hunk_rows.extend(
            self.rows
                .iter()
                .enumerate()
                .filter_map(|(index, row)| matches!(row, Row::HunkHeader { .. }).then_some(index)),
        );
    }

    fn show_hunk(&mut self, hunk: usize, cx: &mut Context<Self>) {
        if let Some(&row) = self.hunk_rows.get(hunk) {
            self.navigated_hunk = hunk;
            self.scroll.scroll_to_item_strict(row, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn render_toolbar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let mut left = div()
            .flex()
            .flex_1()
            .min_w(px(SIDEBAR / 2.0))
            .items_center()
            .gap(px(SPACE_2))
            .overflow_hidden();
        let mode = match &self.content {
            Content::Text(data) => data.mode,
            Content::FileMessage { mode, .. } => *mode,
            _ => StageMode::ReadOnly,
        };
        if let Content::Text(data) = &self.content {
            let stats = data.diff.stats;
            left = left
                .child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .text_style(&TEXT_BODY_MEDIUM, cx)
                        .text_color(hsla(p.ink))
                        .child(data.path.clone()),
                )
                .children(data.lfs.as_ref().map(|_| Pill::new("LFS").compact()))
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
        left = left.children(match mode {
            StageMode::Stage => Some(Pill::new("Unstaged").compact()),
            StageMode::Unstage => Some(Pill::new("Staged").compact()),
            StageMode::ReadOnly => None,
        });
        let layout_view = cx.entity();
        let has_file = matches!(self.content, Content::Text(_) | Content::FileMessage { .. });
        let can_discard = match &self.content {
            Content::Text(data) => data.mode == StageMode::Stage,
            Content::FileMessage { mode, .. } => *mode == StageMode::Stage,
            _ => false,
        };
        let show_insights = self.show_file_insights && has_file;

        // Compact context dropdown ("3 lines ▾") instead of a wide 3-way segmented control.
        let toggle_ctx = cx.entity();
        let close_ctx = cx.entity();
        let ctx_id = ElementId::from("diff-context-menu");
        let ctx_handle = focus::handle(&ctx_id, window, cx);
        let hover_bg = hsla(p.bg_hover);
        let hover_ink = hsla(p.ink);
        let mut ctx_trigger = div()
            .id(ctx_id)
            .track_focus(&ctx_handle)
            .tab_stop(true)
            .flex()
            .items_center()
            .gap(px(SPACE_1))
            .h(px(ROW_DIFF))
            .px(px(SPACE_2))
            .rounded(px(RADIUS_SM))
            .border_1()
            .border_color(hsla(if self.context_menu_open {
                p.line_control
            } else {
                p.line
            }))
            .bg(hsla(if self.context_menu_open {
                p.bg_hover
            } else {
                p.bg_raised
            }))
            .text_style(&TEXT_SMALL, cx)
            .font_weight(FontWeight(500.0))
            .text_color(hsla(p.ink))
            .cursor_pointer()
            .focus_visible(move |style| focus::inset_ring(style, ring))
            .on_click(move |_, _, cx| {
                toggle_ctx.update(cx, |view, cx| view.toggle_context_menu(cx));
            })
            .child(self.around.label())
            .child(Icon::new(IconName::ChevronDown).small().color(p.ink_muted));
        if !self.context_menu_open {
            ctx_trigger = ctx_trigger.hover(move |s| s.bg(hover_bg).text_color(hover_ink));
        }

        let ctx_popover = self.context_menu_open.then(|| {
            let mut menu = div()
                .id("diff-context-popover")
                .occlude()
                .absolute()
                .top(px(ROW_DIFF + SPACE_1))
                .right(px(0.))
                .flex()
                .flex_col()
                .min_w(px(SIDEBAR - SPACE_8 * 3.0))
                .gap(px(SPACE_1 / 2.0))
                .p(px(SPACE_1))
                .rounded(px(RADIUS_LG))
                .border_1()
                .border_color(hsla(p.line))
                .bg(hsla(p.bg_raised))
                .shadow(shadows(p.shadow_overlay))
                .on_mouse_down_out(move |_, _, cx| {
                    cx.stop_propagation();
                    close_ctx.update(cx, |view, cx| {
                        view.context_menu_open = false;
                        cx.notify();
                    });
                });
            for (index, around) in Around::ALL.into_iter().enumerate() {
                let active = around == self.around;
                let pick = cx.entity();
                let item_hover = hsla(p.bg_hover);
                let mut row = div()
                    .id(ElementId::from(("diff-around-opt", index)))
                    .flex()
                    .items_center()
                    .justify_between()
                    .h(px(ROW_LIST))
                    .px(px(SPACE_2))
                    .rounded(px(RADIUS_MD))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(if active { p.accent } else { p.ink }))
                    .cursor_pointer()
                    .on_click(move |_, _, cx| {
                        pick.update(cx, |view, cx| view.set_around(around, cx));
                    })
                    .child(around.label())
                    .children(active.then_some("✓"));
                row = if active {
                    row.bg(hsla(p.accent_soft))
                } else {
                    row.hover(move |s| s.bg(item_hover))
                };
                menu = menu.child(row);
            }
            deferred(menu)
        });

        // Secondary file actions menu ("⋯": Blame, File history, Discard file).
        let actions_menu = (show_insights || can_discard).then(|| {
            let toggle_actions = cx.entity();
            let close_actions = cx.entity();
            let blame_view = cx.entity();
            let history_view = cx.entity();
            let discard_view = cx.entity();
            let popover = self.actions_menu_open.then(|| {
                let mut menu = div()
                    .id("diff-actions-popover")
                    .occlude()
                    .absolute()
                    .top(px(ROW_DIFF + SPACE_1))
                    .right(px(0.))
                    .flex()
                    .flex_col()
                    .min_w(px(SIDEBAR - SPACE_8 * 2.0))
                    .gap(px(SPACE_1 / 2.0))
                    .p(px(SPACE_1))
                    .rounded(px(RADIUS_LG))
                    .border_1()
                    .border_color(hsla(p.line))
                    .bg(hsla(p.bg_raised))
                    .shadow(shadows(p.shadow_overlay))
                    .on_mouse_down_out(move |_, _, cx| {
                        cx.stop_propagation();
                        close_actions.update(cx, |view, cx| {
                            view.actions_menu_open = false;
                            cx.notify();
                        });
                    });
                if show_insights {
                    menu = menu
                        .child(
                            Button::new("diff-file-blame", "Blame")
                                .icon(IconName::FileText)
                                .ghost()
                                .full_width()
                                .on_click(move |_, _, cx| {
                                    blame_view.update(cx, |view, cx| {
                                        view.actions_menu_open = false;
                                        cx.emit(DiffEvent::Blame);
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            Button::new("diff-file-history", "File history")
                                .icon(IconName::History)
                                .ghost()
                                .full_width()
                                .on_click(move |_, _, cx| {
                                    history_view.update(cx, |view, cx| {
                                        view.actions_menu_open = false;
                                        cx.emit(DiffEvent::FileHistory);
                                        cx.notify();
                                    });
                                }),
                        );
                }
                if show_insights && can_discard {
                    menu = menu.child(div().py(px(SPACE_1 / 2.0)).child(Divider::horizontal()));
                }
                if can_discard {
                    menu = menu.child(
                        Button::new("diff-discard-file", "Discard file")
                            .icon(IconName::Trash)
                            .danger_ghost()
                            .full_width()
                            .on_click(move |_, _, cx| {
                                discard_view.update(cx, |view, cx| {
                                    view.actions_menu_open = false;
                                    cx.emit(DiffEvent::DiscardFile);
                                    cx.notify();
                                });
                            }),
                    );
                }
                deferred(menu)
            });

            div()
                .relative()
                .flex_none()
                .child(
                    Button::new("diff-actions-menu", "")
                        .tooltip("Diff options")
                        .icon(IconName::More)
                        .tooltip("File actions")
                        .ghost()
                        .compact()
                        .on_click(move |_, _, cx| {
                            toggle_actions.update(cx, |view, cx| view.toggle_actions_menu(cx));
                        }),
                )
                .children(popover)
        });

        let controls = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(SPACE_2))
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(ctx_trigger)
                    .children(ctx_popover),
            )
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
            .children(actions_menu);
        let count = self.hunk_rows.len();
        let hunk = self.navigated_hunk;
        let previous = cx.entity();
        let next = cx.entity();
        file_toolbar(cx)
            .child(left)
            .child(controls)
            .children((count > 1).then(|| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(SPACE_1))
                    .child(
                        Button::new("previous-hunk", "Previous change")
                            .ghost()
                            .compact()
                            .disabled(hunk == 0)
                            .on_click(move |_, _, cx| {
                                previous.update(cx, |view, cx| {
                                    view.show_hunk(hunk.saturating_sub(1), cx);
                                })
                            }),
                    )
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(format!("{} / {count}", hunk + 1)),
                    )
                    .child(
                        Button::new("next-hunk", "Next change")
                            .ghost()
                            .compact()
                            .disabled(hunk + 1 >= count)
                            .on_click(move |_, _, cx| {
                                next.update(cx, |view, cx| {
                                    view.show_hunk(hunk + 1, cx);
                                })
                            }),
                    )
            }))
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
    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Div> {
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
        let ring = focus::ring_color(cx);
        let view = cx.entity();
        let data = &*data;
        let rows = &self.rows;
        let selection = &self.selection;
        let number_w = self.number_w;
        range
            .map(|index| match rows.get(index) {
                Some(Row::HunkHeader { hunk }) => hunk_row(data, &view, selection, p, *hunk, cx),
                Some(Row::Unified { hunk, line }) => unified_row(
                    data, &view, selection, number_w, theme, p, ring, *hunk, *line, window, cx,
                ),
                Some(Row::Split { hunk, left, right }) => split_row(
                    data, &view, selection, number_w, theme, p, ring, *hunk, *left, *right, window,
                    cx,
                ),
                None => div().h(px(ROW_DIFF)),
            })
            .collect()
    }
}

/// The shared header surface for the diff, blame, and file history.
pub fn file_toolbar(cx: &App) -> Div {
    div()
        .flex()
        .flex_wrap()
        .flex_none()
        .items_center()
        .gap(px(SPACE_2))
        .min_h(px(TOOLBAR))
        .w_full()
        .px(px(SPACE_4))
        .py(px(SPACE_1))
        .border_b_1()
        .border_color(hsla(cx.palette().line))
}

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let body: AnyElement = match &self.content {
            Content::Loading => self.message("Reading the diff…".into(), cx),
            Content::Message(text) | Content::FileMessage { text, .. } => {
                self.message(text.clone(), cx)
            }
            Content::Text(data) if data.diff.is_empty() => {
                self.message("No differences".into(), cx)
            }
            Content::Text(data) => {
                let lfs_banner = data.lfs.as_ref().map(|lfs| {
                    let pull_view = cx.entity();
                    let mut banner = Banner::new(BannerKind::Warning, lfs.summary.clone());
                    if lfs.is_pointer {
                        banner = banner.action(Button::new("diff-lfs-pull", "Pull LFS").on_click(
                            move |_, _, cx| {
                                pull_view.update(cx, |_, cx| cx.emit(DiffEvent::PullLfs))
                            },
                        ));
                    }
                    banner
                });
                let note = (data.diff.note == Some(DiffNote::TooExpensive)).then(|| {
                    Banner::new(
                        BannerKind::Warning,
                        "This diff was too expensive to compute, so the whole file is shown as replaced.",
                    )
                });
                let has_banners = lfs_banner.is_some() || note.is_some();
                let banner_strip = has_banners.then(|| {
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .gap(px(SPACE_2))
                        .px(px(SPACE_3))
                        .py(px(SPACE_2))
                        .border_b_1()
                        .border_color(hsla(p.line))
                        .children(lfs_banner)
                        .children(note)
                });
                let list = uniform_list(
                    "diff-rows",
                    self.rows.len(),
                    cx.processor(|this, range, window, cx| this.render_rows(range, window, cx)),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .h_full();
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .children(banner_strip)
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
            .child(self.render_toolbar(window, cx))
            .child(body)
    }
}

fn hunk_row(
    data: &DiffData,
    view: &gpui::Entity<DiffView>,
    selection: &BTreeSet<(usize, usize)>,
    p: &tokens::Palette,
    hunk: usize,
    cx: &mut App,
) -> Div {
    let mut row = div()
        .flex()
        .items_center()
        .gap(px(SPACE_2))
        .h(px(ROW_DIFF))
        .w_full()
        .px(px(SPACE_3))
        .bg(hsla(p.bg_raised))
        .text_style(&TEXT_CODE_SMALL, cx)
        .text_color(hsla(p.ink_muted))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .child(hunk_header(&data.diff.hunks[hunk])),
        );
    let verb = match data.mode {
        StageMode::ReadOnly => return row,
        StageMode::Stage => "Stage",
        StageMode::Unstage => "Unstage",
    };
    let selected = selection.range((hunk, 0)..(hunk + 1, 0)).count();
    let all_view = view.clone();
    row = row.child(
        Button::new(("hunk-all", hunk), format!("{verb} hunk"))
            .ghost()
            .compact()
            .tooltip(format!("{verb} all changes in this hunk"))
            .on_click(move |_, _, cx| all_view.update(cx, |view, cx| view.request_hunk(hunk, cx))),
    );
    if selected > 0 {
        let lines_view = view.clone();
        row = row.child(
            Button::new(
                ("hunk-lines", hunk),
                format!("{verb} selected ({selected})"),
            )
            .compact()
            .tooltip(format!("{verb} only the selected lines in this hunk"))
            .on_click(move |_, _, cx| {
                lines_view.update(cx, |view, cx| view.request_lines(hunk, cx))
            }),
        );
    }
    if data.mode == StageMode::Stage {
        if selected > 0 {
            let discard_view = view.clone();
            row = row.child(
                Button::new(
                    ("hunk-discard-lines", hunk),
                    format!("Discard selected ({selected})"),
                )
                .danger_ghost()
                .compact()
                .tooltip("Discard only the selected lines in this hunk")
                .on_click(move |_, _, cx| {
                    discard_view.update(cx, |view, cx| view.request_discard_lines(hunk, cx))
                }),
            );
        }
        let discard_view = view.clone();
        row = row.child(
            Button::new(("hunk-discard", hunk), "Discard hunk")
                .danger_ghost()
                .compact()
                .on_click(move |_, _, cx| {
                    discard_view.update(cx, |view, cx| view.request_discard_hunk(hunk, cx))
                }),
        );
    }
    row
}

/// The checkbox slot of a changed line; context lines and read-only diffs
/// have an empty slot.
#[allow(clippy::too_many_arguments)]
fn select_cell(
    data: &DiffData,
    view: &gpui::Entity<DiffView>,
    selection: &BTreeSet<(usize, usize)>,
    p: &tokens::Palette,
    ring: gpui::Hsla,
    hunk: usize,
    line: usize,
    kind: LineKind,
    window: &mut Window,
    cx: &mut gpui::App,
) -> Div {
    let slot = div()
        .flex_none()
        .w(px(SELECT_SLOT))
        .flex()
        .items_center()
        .justify_center();
    if data.mode == StageMode::ReadOnly || kind == LineKind::Context {
        return slot;
    }
    let on = selection.contains(&(hunk, line));
    let change = &data.diff.hunks[hunk].lines[line];
    let label = if let Some(number) = change.new_no {
        format!("Select added line {number}")
    } else {
        format!("Select removed line {}", change.old_no.unwrap_or_default())
    };
    let view = view.clone();
    let id = ElementId::from((SharedString::from(format!("line-check-{hunk}")), line));
    let handle = focus::handle(&id, window, cx);
    let mark = hsla(p.on_accent);
    let accent = hsla(p.accent);
    let hover_bg = hsla(p.bg_hover);
    let mut box_el = div()
        .id(id)
        .role(Role::CheckBox)
        .aria_label(label)
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .track_focus(&handle)
        .tab_stop(true)
        .flex()
        .items_center()
        .justify_center()
        .size(px(SPACE_3))
        .rounded(px(SPACE_1 / 2.0))
        .border_1()
        .border_color(hsla(if on { p.accent } else { p.line_control }))
        .bg(hsla(if on { p.accent } else { p.bg_panel }))
        .cursor_pointer()
        .focus_visible(move |style| focus::inset_ring(style, ring))
        .on_click(move |_, _, cx| view.update(cx, |view, cx| view.toggle_line(hunk, line, cx)))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    if !on {
                        return;
                    }
                    let mut path = PathBuilder::stroke(px(1.5));
                    let (x, y) = (bounds.origin.x, bounds.origin.y);
                    path.move_to(point(x + px(2.5), y + px(6.)));
                    path.line_to(point(x + px(5.), y + px(8.5)));
                    path.line_to(point(x + px(9.5), y + px(3.5)));
                    if let Ok(path) = path.build() {
                        window.paint_path(path, mark);
                    }
                },
            )
            .size_full(),
        );
    box_el = if on {
        box_el.hover(|style| style.opacity(0.9))
    } else {
        box_el.hover(move |style| style.bg(hover_bg).border_color(accent))
    };
    slot.child(box_el)
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

#[allow(clippy::too_many_arguments)]
fn unified_row(
    data: &DiffData,
    view: &gpui::Entity<DiffView>,
    selection: &BTreeSet<(usize, usize)>,
    nw: f32,
    theme: Theme,
    p: &tokens::Palette,
    ring: gpui::Hsla,
    hunk: usize,
    line: usize,
    window: &mut Window,
    cx: &mut gpui::App,
) -> Div {
    let l = &data.diff.hunks[hunk].lines[line];
    base_row(cx)
        .bg(hsla(fill(l.kind, p)))
        .child(select_cell(
            data, view, selection, p, ring, hunk, line, l.kind, window, cx,
        ))
        .child(number_cell(l.old_no, nw, p))
        .child(number_cell(l.new_no, nw, p))
        .child(sign_cell(l.kind, p))
        .child(code_cell(data, theme, p, l))
}

#[allow(clippy::too_many_arguments)]
fn split_row(
    data: &DiffData,
    view: &gpui::Entity<DiffView>,
    selection: &BTreeSet<(usize, usize)>,
    nw: f32,
    theme: Theme,
    p: &tokens::Palette,
    ring: gpui::Hsla,
    hunk: usize,
    left: Option<usize>,
    right: Option<usize>,
    window: &mut Window,
    cx: &mut gpui::App,
) -> Div {
    let lines = &data.diff.hunks[hunk].lines;
    let half =
        |index: Option<usize>, left_side: bool, window: &mut Window, cx: &mut gpui::App| -> Div {
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
                .child(select_cell(
                    data, view, selection, p, ring, hunk, index, l.kind, window, cx,
                ))
                .child(number_cell(number, nw, p))
                .child(sign_cell(l.kind, p))
                .child(code_cell(data, theme, p, l))
        };
    let left_half = half(left, true, window, cx);
    let right_half = half(right, false, window, cx);
    base_row(cx)
        .child(left_half)
        .child(div().flex_none().w(px(1.)).h_full().bg(hsla(p.line)))
        .child(right_half)
}

/// The text of one line, coloured and with its word-level changes marked.
fn code_cell(data: &DiffData, theme: Theme, p: &tokens::Palette, line: &DiffLine) -> Div {
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
        mode: StageMode::ReadOnly,
        inputs: None,
        lfs: None,
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
        self.selection.clear();
        self.rebuild();
        cx.notify();
    }
}

impl DiffView {
    /// The diff on show, with what a stage request needs.
    pub fn data(&self) -> Option<&DiffData> {
        match &self.content {
            Content::Text(data) => Some(data),
            _ => None,
        }
    }

    /// The selected lines of `hunk`, in order.
    pub fn selected_lines(&self, hunk: usize) -> Vec<usize> {
        self.selection
            .range((hunk, 0)..(hunk + 1, 0))
            .map(|(_, line)| *line)
            .collect()
    }

    fn toggle_line(&mut self, hunk: usize, line: usize, cx: &mut Context<Self>) {
        if !self.selection.remove(&(hunk, line)) {
            self.selection.insert((hunk, line));
        }
        cx.notify();
    }

    fn request_hunk(&mut self, hunk: usize, cx: &mut Context<Self>) {
        self.selection.clear();
        cx.emit(DiffEvent::Hunk { hunk });
        cx.notify();
    }

    fn request_discard_hunk(&mut self, hunk: usize, cx: &mut Context<Self>) {
        self.selection.clear();
        cx.emit(DiffEvent::DiscardHunk { hunk });
        cx.notify();
    }

    fn request_discard_lines(&mut self, hunk: usize, cx: &mut Context<Self>) {
        let lines = self.selected_lines(hunk);
        self.selection.clear();
        if !lines.is_empty() {
            cx.emit(DiffEvent::DiscardLines { hunk, lines });
        }
        cx.notify();
    }

    fn request_lines(&mut self, hunk: usize, cx: &mut Context<Self>) {
        let lines = self.selected_lines(hunk);
        self.selection.clear();
        if !lines.is_empty() {
            cx.emit(DiffEvent::Lines { hunk, lines });
        }
        cx.notify();
    }
}
