//! What one repo tab shows: the summary header, the list of changed files
//! and the diff of the selected one. The state it renders belongs to the
//! `RepoSession`; this view only remembers which file is selected.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Window,
    div, px,
};
use invoke_core::{ReadId, RepoPath, Snapshot, StatusSnapshot};
use shell::Source;
use tokens::{SIDEBAR, SPACE_8};
use ui::convert::hsla;
use ui::{
    ChangeList, DiffContent, DiffView, RepoSummary, Section, Summary, ThemeExt, diff_view,
    list_rows,
};

use crate::diff_load::{self, Loaded};
use crate::session::RepoSession;

/// The changed-file list is a little wider than the repository sidebar.
const CHANGES_W: f32 = SIDEBAR + 2.0 * SPACE_8;

pub struct RepoView {
    pub session: Entity<RepoSession>,
    diff: Entity<DiffView>,
    selected: Option<(Section, RepoPath)>,
    /// The read the list was last reconciled with.
    last_read: Option<ReadId>,
    /// Numbers each diff request; an answer for an older one is dropped.
    request: u64,
}

fn has(status: &StatusSnapshot, section: Section, path: &RepoPath) -> bool {
    status.entries.iter().any(|entry| {
        entry.path == *path
            && match section {
                Section::Staged => entry.staged.is_some(),
                Section::Unstaged => entry.unstaged.is_some(),
            }
    })
}

impl RepoView {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let diff = cx.new(|_| DiffView::new());
        cx.observe(&session, |this, _, cx| this.session_changed(cx))
            .detach();
        Self {
            session,
            diff,
            selected: None,
            last_read: None,
            request: 0,
        }
    }

    /// The session moved. Only a new read can change the list or the diff.
    fn session_changed(&mut self, cx: &mut Context<Self>) {
        let (read, status) = match &self.session.read(cx).state.snapshot {
            Snapshot::Read { read, disk } => (*read, disk.status.clone()),
            Snapshot::NotRead => {
                cx.notify();
                return;
            }
        };
        if self.last_read == Some(read) {
            cx.notify();
            return;
        }
        self.last_read = Some(read);
        match self.selected.clone() {
            // Still changed where it was: show the new content, keep the place.
            Some((section, path)) if has(&status, section, &path) => {
                self.load(section, path, true, cx)
            }
            // Moved to the other half (staged or unstaged away).
            Some((section, path)) => {
                let other = match section {
                    Section::Staged => Section::Unstaged,
                    Section::Unstaged => Section::Staged,
                };
                if has(&status, other, &path) {
                    self.select(other, path, cx);
                } else {
                    self.clear(cx);
                }
            }
            // Nothing chosen yet: open the first file.
            None => {
                if let Some(ui::ListRow::File { section, path, .. }) = list_rows(&status)
                    .into_iter()
                    .find(|row| matches!(row, ui::ListRow::File { .. }))
                {
                    self.select(section, RepoPath(path.to_string()), cx);
                }
            }
        }
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.request += 1;
        self.diff.update(cx, |diff, cx| {
            diff.set_content(DiffContent::Message("No file selected".into()), cx)
        });
    }

    fn select(&mut self, section: Section, path: RepoPath, cx: &mut Context<Self>) {
        self.selected = Some((section, path.clone()));
        self.diff
            .update(cx, |diff, cx| diff.set_content(DiffContent::Loading, cx));
        self.load(section, path, false, cx);
        cx.notify();
    }

    /// Reads and diffs the file on a background thread, then shows it; the
    /// syntax colours follow when the parse is done. `keep_place` leaves the
    /// scroll position alone (a refresh of the same file).
    fn load(&mut self, section: Section, path: RepoPath, keep_place: bool, cx: &mut Context<Self>) {
        self.request += 1;
        let request = self.request;
        let paths = self.session.read(cx).discovered.paths.clone();
        let source = match section {
            Section::Unstaged => Source::IndexToWorktree,
            Section::Staged => Source::HeadToIndex,
        };
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { diff_load::load(&paths, &path, source) }
                })
                .await;
            let name = path.0.clone();
            let texts = this
                .update(cx, |this, cx| {
                    if this.request != request {
                        return None;
                    }
                    match loaded {
                        Loaded::Message(text) => {
                            this.diff.update(cx, |diff, cx| {
                                diff.set_content(DiffContent::Message(text.into()), cx)
                            });
                            None
                        }
                        Loaded::Diff(data) => {
                            let texts = (data.diff.old.clone(), data.diff.new.clone());
                            this.diff.update(cx, |diff, cx| {
                                let content = DiffContent::Text(data);
                                if keep_place {
                                    diff.replace_content(content, cx)
                                } else {
                                    diff.set_content(content, cx)
                                }
                            });
                            Some(texts)
                        }
                    }
                })
                .ok()
                .flatten();
            let Some((old, new)) = texts else { return };
            let spans = cx
                .background_executor()
                .spawn({
                    let name = name.clone();
                    async move { diff_view::highlight_sides(&name, &old, &new) }
                })
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

impl Render for RepoView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let session = self.session.read(cx);
        let mut summary = RepoSummary::new(
            Summary::of(&session.state),
            session.discovered.paths.workdir.display().to_string(),
        );
        if let Some(error) = &session.watch_error {
            summary = summary.warning(format!(
                "Changes made outside Invoke Git will not show up on their own ({error}). Press F5 to refresh."
            ));
        }
        let rows = match &session.state.snapshot {
            Snapshot::Read { disk, .. } => list_rows(&disk.status),
            Snapshot::NotRead => Vec::new(),
        };
        let selected: Option<(Section, SharedString)> = self
            .selected
            .as_ref()
            .map(|(section, path)| (*section, path.0.clone().into()));
        let this = cx.entity();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(summary)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .w(px(CHANGES_W))
                            .py(px(tokens::SPACE_2))
                            .bg(hsla(p.bg_panel))
                            .border_r_1()
                            .border_color(hsla(p.line))
                            .child(ChangeList::new(
                                rows,
                                selected,
                                move |section, path, _, cx| {
                                    this.update(cx, |this, cx| {
                                        this.select(section, RepoPath(path.to_string()), cx)
                                    })
                                },
                            )),
                    )
                    .child(div().flex_1().min_w(px(0.)).child(self.diff.clone())),
            )
    }
}
