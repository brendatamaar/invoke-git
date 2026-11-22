//! The commit detail of the History page (task 19): what the selected
//! commit says and changed. It reads the commit and the sides of the chosen
//! file on a background thread; an answer for a commit or file that is no
//! longer the chosen one is dropped.

use gpui::{
    AppContext, ClipboardItem, Context, Entity, IntoElement, ParentElement, Render, Styled, Window,
    div, px,
};
use invoke_core::{FileChange, RepoPath};
use shell::InputsError;
use shell::commit_detail::{Change, CommitDetail as Detail, read_commit, read_commit_sides};
use shell::history::CommitId;
use tokens::TEXT_BODY;
use ui::convert::hsla;
use ui::time::format_date_time;
use ui::{
    CommitDetail, CommitFile, CommitInfo, DiffContent, DiffView, TextStyleExt, ThemeExt, diff_view,
    split_message,
};

use crate::session::RepoSession;

/// Phases are an enum: a pane that is loading has no commit to show.
enum Phase {
    Nothing,
    Loading,
    Ready {
        info: CommitInfo,
        selected: Option<usize>,
    },
    Failed(String),
}

pub struct CommitPane {
    session: Entity<RepoSession>,
    diff: Entity<DiffView>,
    phase: Phase,
    /// The commit asked for last; answers for another are dropped.
    wanted: Option<CommitId>,
    /// Numbers each file load.
    request: u64,
}

fn file_change(change: Change) -> FileChange {
    match change {
        Change::Added => FileChange::Added,
        Change::Modified => FileChange::Modified,
        Change::Deleted => FileChange::Deleted,
        Change::TypeChanged => FileChange::TypeChanged,
    }
}

/// What the panel prints for `detail`.
pub fn info_of(detail: &Detail) -> CommitInfo {
    let (subject, body) = split_message(&detail.message);
    CommitInfo {
        hash: detail.id.clone().into(),
        subject: subject.into(),
        body: body.into(),
        author: detail.author.clone().into(),
        date: format_date_time(detail.seconds, detail.offset).into(),
        parents: detail.parents.iter().map(|p| p.clone().into()).collect(),
        files: detail
            .files
            .iter()
            .map(|file| CommitFile {
                path: file.path.clone().into(),
                change: file_change(file.change),
            })
            .collect(),
    }
}

impl CommitPane {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        Self {
            session,
            diff: cx.new(|_| DiffView::new()),
            phase: Phase::Nothing,
            wanted: None,
            request: 0,
        }
    }

    /// Shows `id` (or nothing).
    pub fn show(&mut self, id: Option<CommitId>, cx: &mut Context<Self>) {
        if id == self.wanted && !matches!(self.phase, Phase::Failed(_)) {
            return;
        }
        self.wanted = id;
        self.request += 1;
        let Some(id) = id else {
            self.phase = Phase::Nothing;
            cx.notify();
            return;
        };
        self.phase = Phase::Loading;
        self.diff
            .update(cx, |diff, cx| diff.set_content(DiffContent::Loading, cx));
        cx.notify();
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { read_commit(&paths, &id) })
                .await;
            this.update(cx, |this, cx| {
                if this.wanted != Some(id) {
                    return;
                }
                match read {
                    Ok(detail) => {
                        let info = info_of(&detail);
                        let first = (!info.files.is_empty()).then_some(0);
                        this.phase = Phase::Ready {
                            info,
                            selected: first,
                        };
                        match first {
                            Some(index) => this.load_file(index, cx),
                            None => this.diff.update(cx, |diff, cx| {
                                diff.set_content(
                                    DiffContent::Message("No file to show.".into()),
                                    cx,
                                )
                            }),
                        }
                    }
                    Err(error) => {
                        this.phase = Phase::Failed(format!("Could not read the commit: {error}"))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Phase::Ready { selected, info } = &mut self.phase {
            if index >= info.files.len() {
                return;
            }
            *selected = Some(index);
        } else {
            return;
        }
        self.diff
            .update(cx, |diff, cx| diff.set_content(DiffContent::Loading, cx));
        self.load_file(index, cx);
        cx.notify();
    }

    /// Reads, diffs and highlights file `index` of the shown commit.
    fn load_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let (Some(id), Phase::Ready { info, .. }) = (self.wanted, &self.phase) else {
            return;
        };
        let Some(file) = info.files.get(index) else {
            return;
        };
        let path = RepoPath(file.path.to_string());
        self.request += 1;
        let request = self.request;
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        match read_commit_sides(&paths, &id, &path) {
                            Ok(sides) => Ok(diff_view::compute(
                                &path.0,
                                sides.old.as_deref().unwrap_or(""),
                                sides.new.as_deref().unwrap_or(""),
                            )),
                            Err(InputsError::NotText(reason)) => {
                                Err(format!("{reason}. There is no text diff to show."))
                            }
                            Err(InputsError::Moved) => {
                                Err("The repository changed. Try again.".into())
                            }
                            Err(InputsError::Read(error)) => {
                                Err(format!("Could not read {}: {error}", path.0))
                            }
                        }
                    }
                })
                .await;
            let texts = this
                .update(cx, |this, cx| {
                    if this.request != request {
                        return None;
                    }
                    match loaded {
                        Ok(data) => {
                            let texts = (data.diff.old.clone(), data.diff.new.clone());
                            this.diff.update(cx, |diff, cx| {
                                diff.set_content(DiffContent::Text(Box::new(data)), cx)
                            });
                            Some(texts)
                        }
                        Err(message) => {
                            this.diff.update(cx, |diff, cx| {
                                diff.set_content(DiffContent::Message(message.into()), cx)
                            });
                            None
                        }
                    }
                })
                .ok()
                .flatten();
            let Some((old, new)) = texts else { return };
            let name = path.0.clone();
            let spans = cx
                .background_executor()
                .spawn(async move { diff_view::highlight_sides(&name, &old, &new) })
                .await;
            this.update(cx, |this, cx| {
                if this.request == request {
                    this.diff
                        .update(cx, |diff, cx| diff.set_spans(spans.0, spans.1, cx));
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Render for CommitPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let message = |text: String, cx: &mut Context<Self>| {
            div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .p(px(16.))
                .bg(hsla(p.bg_panel))
                .text_style(&TEXT_BODY, cx)
                .text_color(hsla(p.ink_muted))
                .child(text)
                .into_any_element()
        };
        match &self.phase {
            Phase::Nothing => message("Select a commit to see what it changed.".into(), cx),
            Phase::Loading => message("Reading the commit…".into(), cx),
            Phase::Failed(error) => message(error.clone(), cx),
            Phase::Ready { info, selected } => {
                let this = cx.entity();
                let hash = info.hash.to_string();
                CommitDetail::new(
                    info.clone(),
                    *selected,
                    move |index, _, cx| this.update(cx, |pane, cx| pane.select_file(index, cx)),
                    move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(hash.clone())),
                )
                .diff(self.diff.clone())
                .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shell::commit_detail::ChangedFile;

    fn detail() -> Detail {
        Detail {
            id: "0123456789abcdef0123456789abcdef01234567".into(),
            parents: vec!["aaaaaaa1111".into(), "bbbbbbb2222".into()],
            author: "Ada".into(),
            email: "ada@example.com".into(),
            seconds: 1_709_608_830,
            offset: 7 * 3600,
            message: "Fix the thing\n\nIt was broken.\n".into(),
            files: vec![
                ChangedFile {
                    path: "a.rs".into(),
                    change: Change::Added,
                },
                ChangedFile {
                    path: "b.rs".into(),
                    change: Change::TypeChanged,
                },
            ],
        }
    }

    #[test]
    fn the_panel_prints_what_the_commit_says() {
        let info = info_of(&detail());
        assert_eq!(info.subject.as_ref(), "Fix the thing");
        assert_eq!(info.body.as_ref(), "It was broken.");
        assert_eq!(info.date.as_ref(), "2024-03-05 10:20 +07:00");
        assert!(info.is_merge());
        assert_eq!(info.parents.len(), 2);
        assert_eq!(info.files[0].change, FileChange::Added);
        assert_eq!(info.files[1].change, FileChange::TypeChanged);
        assert_eq!(info.files[1].path.as_ref(), "b.rs");
    }
}
