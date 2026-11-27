//! The commit detail of the History page (task 19): what the selected
//! commit says and changed. It reads the commit and the sides of the chosen
//! file on a background thread; an answer for a commit or file that is no
//! longer the chosen one is dropped.

use gpui::{
    AppContext, ClipboardItem, Context, Entity, EventEmitter, IntoElement, ParentElement, Render,
    Styled, Window, div, px,
};
use invoke_core::{Event, FileChange, Head, Oid, RepoPath, Snapshot};
use shell::InputsError;
use shell::commit_detail::{Change, CommitDetail as Detail, read_commit, read_commit_sides};
use shell::history::CommitId;
use tokens::TEXT_BODY;
use ui::components::Button;
use ui::convert::hsla;
use ui::time::format_date_time;
use ui::{
    CommitDetail, CommitFile, CommitInfo, DiffContent, DiffView, TextStyleExt, ThemeExt, diff_view,
    split_message,
};

use crate::file_insights::{FileMode, FileVersion, file_actions};
use crate::rebase_view::rebase_target_allowed;
use crate::session::RepoSession;

pub struct InspectFile(pub FileVersion, pub FileMode);
impl EventEmitter<InspectFile> for CommitPane {}

pub struct StartRebase(pub Oid);
impl EventEmitter<StartRebase> for CommitPane {}

pub(crate) fn rebase_button(
    oid: Oid,
    session: &RepoSession,
    on_start: impl Fn(Oid, &mut gpui::App) + 'static,
) -> Button {
    let allowed = matches!(
        &session.state.snapshot,
        Snapshot::Read { disk, .. } if rebase_target_allowed(disk, &oid)
    );
    let label = format!("Interactive rebase onto {}…", ui::short_hash(oid.as_hex()));
    Button::new("interactive-rebase-onto", label)
        .ghost()
        .disabled(!allowed)
        .on_click(move |_, _, cx| on_start(oid.clone(), cx))
}

pub(crate) fn replay_buttons(
    oid: Oid,
    session: &RepoSession,
    send: impl Fn(Event, &mut gpui::App) + Clone + 'static,
) -> [Button; 2] {
    let clean = matches!(
        &session.state.snapshot,
        Snapshot::Read { disk, .. } if disk.phase == invoke_core::Phase::Clean
    );
    let target = match session.state.snapshot.disk().map(|disk| &disk.head) {
        Some(Head::Branch { name, .. } | Head::Unborn(name)) => name.0.as_str(),
        _ => "HEAD",
    };
    let commit = ui::short_hash(oid.as_hex());
    [
        (
            "cherry-pick-commit",
            format!("Cherry-pick {commit} onto {target}"),
            Event::CherryPickRequested(oid.clone()),
        ),
        (
            "revert-commit",
            format!("Revert {commit} on {target}"),
            Event::RevertRequested(oid),
        ),
    ]
    .map(|(id, label, event)| {
        let send = send.clone();
        Button::new(id, label)
            .ghost()
            .disabled(!clean)
            .on_click(move |_, _, cx| send(event.clone(), cx))
    })
}

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
                .p(px(tokens::SPACE_4))
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
                let session = self.session.clone();
                let actions = Oid::from_hex(&hash).ok().map(|oid| {
                    let rebase_ent = cx.entity();
                    let rebase =
                        rebase_button(oid.clone(), self.session.read(cx), move |id, cx| {
                            rebase_ent.update(cx, |_, cx| cx.emit(StartRebase(id)));
                        });
                    replay_buttons(oid, self.session.read(cx), move |event, cx| {
                        session.update(cx, |session, cx| session.deliver(event, cx));
                    })
                    .into_iter()
                    .chain(std::iter::once(rebase))
                });
                let file = selected.and_then(|ix| info.files.get(ix)).and_then(|file| {
                    let revision = if matches!(file.change, FileChange::Deleted) {
                        info.parents.first()?.to_string()
                    } else {
                        hash.clone()
                    };
                    Some(FileVersion {
                        path: file.path.to_string(),
                        revision: Some(revision),
                    })
                });
                let inspect = cx.entity();
                let file_buttons = file.map(|file| {
                    file_actions(move |mode, cx| {
                        inspect.update(cx, |_, cx| cx.emit(InspectFile(file.clone(), mode)));
                    })
                });
                CommitDetail::new(
                    info.clone(),
                    *selected,
                    move |index, _, cx| this.update(cx, |pane, cx| pane.select_file(index, cx)),
                    move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(hash.clone())),
                )
                .actions(actions.into_iter().flatten())
                .actions(file_buttons.into_iter().flatten())
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
