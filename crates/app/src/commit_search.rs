//! Incremental search of the commits reachable from the history graph.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use gpui::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, Styled, Task, UniformListScrollHandle, Window, deferred, div, px,
};
use invoke_core::search::{QueryError, SearchField, SearchQuery};
use shell::{
    RepoPaths,
    history::{CommitId, read_meta},
    search::{SearchEvent, search},
};
use tokens::{RADIUS_LG, ROW_DIFF, ROW_LIST, SIDEBAR, SPACE_1, SPACE_2, SPACE_8, TEXT_SMALL};
use ui::blame_view::{CommitList, CommitListRow};
use ui::convert::{hsla, shadows};
use ui::time::format_date_time;
use ui::{Button, EditorEvent, Icon, IconName, TextEditor, TextStyleExt, ThemeExt};

const FIELDS: [(SearchField, &str); 4] = [
    (SearchField::Message, "Message"),
    (SearchField::Author, "Author"),
    (SearchField::Hash, "Hash"),
    (SearchField::Path, "Path"),
];
const RESULT_LIMIT: usize = 500;
const DEBOUNCE: Duration = Duration::from_millis(180);

pub struct CommitSelected(pub CommitId);

enum Status {
    Empty,
    Invalid(QueryError),
    Scanning,
    Complete,
    Limited,
    Cancelled,
    Failed(String),
}

pub struct CommitSearch {
    paths: RepoPaths,
    editor: Entity<TextEditor>,
    field: usize,
    open: bool,
    field_menu_open: bool,
    visible: bool,
    status: Status,
    scanned: usize,
    rows: Arc<[CommitListRow]>,
    scroll: UniformListScrollHandle,
    cancel: Arc<AtomicBool>,
    task: Option<Task<()>>,
}

impl EventEmitter<CommitSelected> for CommitSearch {}

impl CommitSearch {
    pub fn new(paths: RepoPaths, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| TextEditor::new("Search commits…", 1, cx).single_line());
        cx.subscribe(&editor, |this, _, _: &EditorEvent, cx| this.refresh(cx))
            .detach();
        Self {
            paths,
            editor,
            field: 0,
            open: false,
            field_menu_open: false,
            visible: false,
            status: Status::Empty,
            scanned: 0,
            rows: Arc::from([]),
            scroll: UniformListScrollHandle::new(),
            cancel: Arc::default(),
            task: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.field_menu_open = false;
        self.editor.update(cx, |ed, cx| ed.focus(window, cx));
        self.refresh(cx);
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        self.open = false;
        self.field_menu_open = false;
        self.stop();
        self.editor.update(cx, |ed, cx| ed.set_text("", cx));
        self.rows = Arc::from([]);
        self.scanned = 0;
        self.status = Status::Empty;
        cx.notify();
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.close(cx);
        } else {
            self.open(window, cx);
        }
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.task = None;
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            self.refresh(cx);
        }
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.stop();
        self.rows = Arc::from([]);
        self.scanned = 0;
        self.scroll = UniformListScrollHandle::new();
        self.status = Status::Empty;
        cx.notify();
        if !self.visible || !self.open {
            return;
        }
        let query = match SearchQuery::parse(FIELDS[self.field].0, self.editor.read(cx).text()) {
            Ok(query) => query,
            Err(QueryError::Empty) => return,
            Err(error) => {
                self.status = Status::Invalid(error);
                return;
            }
        };
        self.status = Status::Scanning;
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let paths = self.paths.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            executor.timer(DEBOUNCE).await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let mut inbox = search(paths.clone(), query, cancel.clone());
            let mut remaining = RESULT_LIMIT;
            loop {
                let paths = paths.clone();
                let (back, event, rows) = executor
                    .spawn(async move {
                        let event = inbox.recv().ok();
                        let rows = match &event {
                            Some(SearchEvent::Page { matches, .. }) => {
                                result_rows(&paths, &matches[..matches.len().min(remaining)])
                            }
                            _ => Vec::new(),
                        };
                        (inbox, event, rows)
                    })
                    .await;
                inbox = back;
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                remaining -= rows.len();
                let keep_reading = matches!(event, Some(SearchEvent::Page { .. })) && remaining > 0;
                if this
                    .update(cx, |this, cx| {
                        match event {
                            Some(SearchEvent::Page { scanned, .. }) => {
                                this.scanned += scanned;
                                if !rows.is_empty() {
                                    let mut combined = this.rows.to_vec();
                                    combined.extend(rows);
                                    this.rows = combined.into();
                                }
                                if remaining == 0 {
                                    this.status = Status::Limited;
                                }
                            }
                            Some(SearchEvent::Done) => this.status = Status::Complete,
                            Some(SearchEvent::Failed(error)) => this.status = Status::Failed(error),
                            None => {
                                this.status = Status::Failed("Search stopped unexpectedly.".into())
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                    || !keep_reading
                {
                    break;
                }
            }
            cancel.store(true, Ordering::Relaxed);
        }));
    }

    fn summary(&self) -> Option<String> {
        Some(match &self.status {
            Status::Empty => return None,
            Status::Invalid(QueryError::InvalidHash) => {
                "Enter a hexadecimal hash or prefix.".into()
            }
            Status::Invalid(QueryError::InvalidPath) => {
                "Enter a repository-relative path with / separators.".into()
            }
            Status::Invalid(QueryError::Empty) => return None,
            Status::Scanning => format!(
                "Searched {} commits · {} matches",
                self.scanned,
                self.rows.len()
            ),
            Status::Complete if self.rows.is_empty() => "No matching commits.".into(),
            Status::Complete => format!("{} matches", self.rows.len()),
            Status::Limited => format!(
                "Showing the first {RESULT_LIMIT} matches. Refine the search for more specific results."
            ),
            Status::Cancelled => "Search cancelled.".into(),
            Status::Failed(error) => error.clone(),
        })
    }
}

fn result_rows(paths: &RepoPaths, ids: &[CommitId]) -> Vec<CommitListRow> {
    if ids.is_empty() {
        return Vec::new();
    }
    let metadata = read_meta(paths, ids).unwrap_or_default();
    ids.iter()
        .enumerate()
        .map(|(ix, id)| {
            let meta = metadata.get(ix).and_then(Option::as_ref);
            CommitListRow {
                id: id.to_string().into(),
                subject: meta.map_or_else(
                    || "Commit details unavailable".into(),
                    |meta| meta.subject.clone().into(),
                ),
                details: meta.map_or_else(gpui::SharedString::default, |meta| {
                    format!(
                        "{} · {}",
                        meta.author,
                        format_date_time(meta.seconds, meta.offset)
                    )
                    .into()
                }),
            }
        })
        .collect()
}

impl Drop for CommitSearch {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Render for CommitSearch {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        let p = cx.palette();
        let toggle_field = cx.entity();
        let close_field = cx.entity();
        let close_btn = cx.entity();
        let key_close = cx.entity();
        let cancel = cx.entity();
        let pick_commit = cx.entity();
        let rows = self.rows.len();

        let field_popover = self.field_menu_open.then(|| {
            let mut menu = div()
                .id("commit-search-field-popover")
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
                    close_field.update(cx, |this, cx| {
                        this.field_menu_open = false;
                        cx.notify();
                    });
                });
            for (index, (_, label)) in FIELDS.iter().enumerate() {
                let active = index == self.field;
                let pick = cx.entity();
                let item = Button::new(("commit-search-opt", index), *label)
                    .full_width()
                    .on_click(move |_, _, cx| {
                        pick.update(cx, |this, cx| {
                            this.field = index;
                            this.field_menu_open = false;
                            this.refresh(cx);
                        });
                    });
                let item = if active { item } else { item.ghost() };
                menu = menu.child(item);
            }
            deferred(menu)
        });

        div()
            .flex()
            .flex_col()
            .flex_none()
            .bg(hsla(p.bg_panel))
            .border_b_1()
            .border_color(hsla(p.line))
            .on_key_down(move |event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    key_close.update(cx, |this, cx| this.close(cx));
                }
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .p(px(SPACE_2))
                    .gap(px(SPACE_2))
                    .child(Icon::new(IconName::Search).small().color(p.ink_muted))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(ROW_LIST * 4.))
                            .child(self.editor.clone()),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_none()
                            .child(
                                Button::new(
                                    "commit-search-field-btn",
                                    format!("{} ▾", FIELDS[self.field].1),
                                )
                                .tooltip("Search field")
                                .compact()
                                .on_click(move |_, _, cx| {
                                    toggle_field.update(cx, |this, cx| {
                                        this.field_menu_open = !this.field_menu_open;
                                        cx.notify();
                                    });
                                }),
                            )
                            .children(field_popover),
                    )
                    .child(
                        Button::new("commit-search-close", "")
                            .tooltip("Close commit search")
                            .icon(IconName::Close)
                            .ghost()
                            .compact()
                            .on_click(move |_, _, cx| {
                                close_btn.update(cx, |this, cx| this.close(cx));
                            }),
                    ),
            )
            .children(self.summary().map(|summary| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(SPACE_2))
                    .px(px(SPACE_2))
                    .pb(px(SPACE_2))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(div().flex_1().child(summary))
                    .children(matches!(self.status, Status::Scanning).then(|| {
                        Button::new("cancel-commit-search", "Cancel")
                            .ghost()
                            .compact()
                            .on_click(move |_, _, cx| {
                                cancel.update(cx, |this, cx| {
                                    this.stop();
                                    this.status = Status::Cancelled;
                                    cx.notify();
                                })
                            })
                    }))
            }))
            .children((rows > 0).then(|| {
                div()
                    .h(px(rows.min(6) as f32 * ROW_LIST))
                    .px(px(SPACE_2))
                    .pb(px(SPACE_2))
                    .child(CommitList::new(
                        self.rows.clone(),
                        self.scroll.clone(),
                        move |id, _, cx| {
                            if let Ok(id) = CommitId::from_hex(id.as_bytes()) {
                                pick_commit.update(cx, |_, cx| cx.emit(CommitSelected(id)));
                            }
                        },
                    ))
            }))
            .into_any_element()
    }
}
