//! File inspection reads are isolated from the diff and can be cancelled
//! when the file, mode, or page changes.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    Context, EventEmitter, IntoElement, ParentElement, Render, Styled, Task,
    UniformListScrollHandle, Window, div, px,
};
use shell::blame::{BlameReader, ReadError};
use shell::history::CommitId;
use tokens::{SPACE_2, SPACE_3, TEXT_BODY_MEDIUM, TEXT_SMALL, TOOLBAR};
use ui::blame_view::{BlameData, BlameRow, BlameView, CommitList, CommitListRow};
use ui::time::format_date_time;
use ui::{Button, Divider, Segmented, TextStyleExt, ThemeExt, convert::hsla};

#[derive(Clone, Copy)]
pub enum FileMode {
    Blame,
    History,
}

#[derive(Clone)]
pub struct FileVersion {
    pub path: String,
    /// `None` means the worktree for blame, HEAD for file history.
    pub revision: Option<String>,
}

pub enum FileInsightsEvent {
    Back,
    Commit(CommitId),
}

pub fn file_actions(open: impl Fn(FileMode, &mut gpui::App) + Clone + 'static) -> [Button; 2] {
    [
        ("file-blame", "Blame", FileMode::Blame),
        ("file-history", "File history", FileMode::History),
    ]
    .map(|(id, label, mode)| {
        let open = open.clone();
        Button::new(id, label)
            .ghost()
            .on_click(move |_, _, cx| open(mode, cx))
    })
}

enum Content {
    Loading,
    Blame(Arc<BlameData>),
    History(Arc<[CommitListRow]>),
    Failed(String),
}

pub struct FileInsights {
    reader: BlameReader,
    file: Option<FileVersion>,
    mode: FileMode,
    content: Content,
    scroll: UniformListScrollHandle,
    cancel: Arc<AtomicBool>,
    task: Option<Task<()>>,
}

impl EventEmitter<FileInsightsEvent> for FileInsights {}

impl FileInsights {
    pub fn new(reader: BlameReader) -> Self {
        Self {
            reader,
            file: None,
            mode: FileMode::Blame,
            content: Content::Loading,
            scroll: UniformListScrollHandle::new(),
            cancel: Arc::default(),
            task: None,
        }
    }

    pub fn open(&mut self, file: FileVersion, mode: FileMode, cx: &mut Context<Self>) {
        self.close();
        self.file = Some(file.clone());
        self.mode = mode;
        self.scroll = UniformListScrollHandle::new();
        let reader = self.reader.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let worker_cancel = cancel.clone();
        let work = cx.background_executor().spawn(async move {
            let FileVersion { path, revision } = file;
            match mode {
                FileMode::Blame => reader
                    .blame_blocking(revision.as_deref(), &path, &worker_cancel)
                    .map(|lines| {
                        let rows = lines
                            .into_iter()
                            .map(|line| BlameRow {
                                commit: line.is_committed().then(|| line.commit.into()),
                                author: line.author.into(),
                                date: format_date_time(line.seconds, line.offset).into(),
                                line: line.line,
                                text: line.text.into(),
                            })
                            .collect();
                        Content::Blame(Arc::new(BlameData::new(&path, rows, &|| {
                            worker_cancel.load(Ordering::Relaxed)
                        })))
                    }),
                FileMode::History => reader
                    .file_history_blocking(revision.as_deref(), &path, None, &worker_cancel)
                    .map(|commits| {
                        Content::History(
                            commits
                                .into_iter()
                                .map(|commit| CommitListRow {
                                    id: commit.id.into(),
                                    subject: commit.summary.into(),
                                    details: format!(
                                        "{} · {} · {}",
                                        commit.author,
                                        format_date_time(commit.seconds, commit.offset),
                                        commit.path
                                    )
                                    .into(),
                                })
                                .collect(),
                        )
                    }),
            }
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let content = match work.await {
                Ok(content) => content,
                Err(ReadError::Failed(error)) => Content::Failed(error),
                Err(ReadError::Cancelled) => return,
            };
            if !cancel.load(Ordering::Relaxed) {
                this.update(cx, |this, cx| {
                    this.content = content;
                    cx.notify();
                })
                .ok();
            }
        }));
        cx.notify();
    }

    pub fn close(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.file = None;
        self.task = None;
        self.content = Content::Loading;
    }
}

impl Drop for FileInsights {
    fn drop(&mut self) {
        self.close();
    }
}

impl Render for FileInsights {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let this = cx.entity();
        let open = move |id: gpui::SharedString, _: &mut Window, cx: &mut gpui::App| {
            if let Ok(id) = CommitId::from_hex(id.as_bytes()) {
                this.update(cx, |_, cx| cx.emit(FileInsightsEvent::Commit(id)));
            }
        };
        let message = |text: String| {
            div()
                .p(px(SPACE_3))
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink_muted))
                .child(text)
                .into_any_element()
        };
        let body = match &self.content {
            Content::Loading => message("Reading file…".into()),
            Content::Failed(error) => message(error.clone()),
            Content::Blame(data) if data.is_empty() => message("This file has no lines.".into()),
            Content::Blame(data) => {
                BlameView::new(data.clone(), self.scroll.clone(), open).into_any_element()
            }
            Content::History(rows) if rows.is_empty() => {
                message("No commits have changed this file.".into())
            }
            Content::History(rows) => {
                CommitList::new(rows.clone(), self.scroll.clone(), open).into_any_element()
            }
        };
        let heading = self
            .file
            .as_ref()
            .map(|file| {
                let source =
                    file.revision
                        .as_deref()
                        .map(ui::short_hash)
                        .unwrap_or(match self.mode {
                            FileMode::Blame => "Working tree",
                            FileMode::History => "HEAD",
                        });
                format!("{} · {source}", file.path)
            })
            .unwrap_or_default();
        let switch = cx.entity();
        let back = cx.entity();
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.))
            .bg(hsla(p.bg_panel))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(SPACE_2))
                    .h(px(TOOLBAR))
                    .px(px(SPACE_3))
                    .border_b_1()
                    .border_color(hsla(p.line))
                    .child(Button::new("file-insights-back", "Back").ghost().on_click(
                        move |_, _, cx| back.update(cx, |_, cx| cx.emit(FileInsightsEvent::Back)),
                    ))
                    .child(Divider::vertical())
                    .child(Segmented::new(
                        "file-insight-mode",
                        vec!["Blame".into(), "File history".into()],
                        match self.mode {
                            FileMode::Blame => 0,
                            FileMode::History => 1,
                        },
                        move |index, _, cx| {
                            switch.update(cx, |this, cx| {
                                if let Some(file) = this.file.clone() {
                                    this.open(
                                        file,
                                        if index == 0 {
                                            FileMode::Blame
                                        } else {
                                            FileMode::History
                                        },
                                        cx,
                                    );
                                }
                            })
                        },
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_style(&TEXT_BODY_MEDIUM, cx)
                            .text_color(hsla(p.ink))
                            .child(heading),
                    ),
            )
            .child(div().flex_1().min_h(px(0.)).child(body))
    }
}
