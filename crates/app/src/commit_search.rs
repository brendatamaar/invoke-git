//! Incremental search of the commits reachable from the history graph.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use gpui::{
    AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, Styled, Task,
    UniformListScrollHandle, Window, div, px,
};
use invoke_core::search::{QueryError, SearchField, SearchQuery};
use shell::{
    RepoPaths,
    history::{CommitId, read_meta},
    search::{SearchEvent, search},
};
use tokens::{ROW_LIST, SPACE_2, TEXT_SMALL};
use ui::blame_view::{CommitList, CommitListRow};
use ui::time::format_date_time;
use ui::{Button, EditorEvent, Segmented, TextEditor, TextStyleExt, ThemeExt, convert::hsla};

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
        let editor = cx.new(|cx| TextEditor::new("Search commits", 1, cx).single_line());
        cx.subscribe(&editor, |this, _, _: &EditorEvent, cx| this.refresh(cx))
            .detach();
        Self {
            paths,
            editor,
            field: 0,
            visible: false,
            status: Status::Empty,
            scanned: 0,
            rows: Arc::from([]),
            scroll: UniformListScrollHandle::new(),
            cancel: Arc::default(),
            task: None,
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
        if !self.visible {
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
        let p = cx.palette();
        let pick_field = cx.entity();
        let cancel = cx.entity();
        let pick_commit = cx.entity();
        let rows = self.rows.len();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .border_b_1()
            .border_color(hsla(p.line))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .p(px(SPACE_2))
                    .gap(px(SPACE_2))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(ROW_LIST * 5.))
                            .child(self.editor.clone()),
                    )
                    .child(Segmented::new(
                        "commit-search-field",
                        FIELDS.iter().map(|(_, label)| (*label).into()).collect(),
                        self.field,
                        move |index, _, cx| {
                            pick_field.update(cx, |this, cx| {
                                this.field = index;
                                this.refresh(cx);
                            })
                        },
                    )),
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
    }
}
