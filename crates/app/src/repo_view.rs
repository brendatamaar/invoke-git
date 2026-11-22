//! What one repo tab shows: the summary header with the Changes and History
//! switch, then either the status tree beside the diff of the selected file
//! (with staging of files, hunks and lines) or the commit graph. The state it
//! renders belongs to the `RepoSession`; this view remembers which file is
//! selected, which folders are folded, and the files still to stage.

use std::collections::{HashSet, VecDeque};

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled, Window,
    div, px,
};
use invoke_core::{Event, ReadId, RepoPath, Snapshot, StatusSnapshot};
use tokens::{SIDEBAR, SPACE_2, SPACE_8, TEXT_SMALL};
use ui::convert::hsla;
use ui::{
    Banner, BannerKind, Button, DiffContent, DiffEvent, DiffView, RepoSummary, Section, Segmented,
    StatusTree, Summary, TextStyleExt, ThemeExt, TreeAction, TreeRow, diff_view, tree_rows,
};

use crate::branch_panel::BranchPanel;
use crate::commit_box::CommitBox;
use crate::diff_load::{self, Loaded, Verb};
use crate::discard::{self, Confirm, What};
use crate::history_view::HistoryPane;
use crate::notice::{self, Tone};
use crate::session::RepoSession;
use crate::staging::{self, Part};

/// The changed-file tree is a little wider than the repository sidebar.
const CHANGES_W: f32 = SIDEBAR + 2.0 * SPACE_8;

/// The two pages of a repo tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Changes,
    History,
}

/// Files to stage or unstage one after another. Each stage is built from a
/// fresh read and carries the index id it expects, so the next one waits for
/// the previous to finish and for the repo to be read again.
enum Bulk {
    Idle,
    /// The next request is being built on a background thread.
    Preparing,
    /// A request was handed to the core; waiting for the session to settle.
    Running,
}

pub struct RepoView {
    pub session: Entity<RepoSession>,
    page: Page,
    history: Entity<HistoryPane>,
    commit: Entity<CommitBox>,
    branches: Entity<BranchPanel>,
    diff: Entity<DiffView>,
    selected: Option<(Section, RepoPath)>,
    collapsed: HashSet<(Section, String)>,
    queue: VecDeque<(Verb, Section, RepoPath)>,
    /// A discard waiting for the user's yes.
    confirm: Option<Confirm>,
    bulk: Bulk,
    /// Something the app itself has to say (a file that cannot be staged).
    local_notice: Option<String>,
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

/// Every file of `section` at or below `dir` (all of them when `dir` is
/// `None`).
fn files_of(status: &StatusSnapshot, section: Section, dir: Option<&str>) -> Vec<RepoPath> {
    let entries: Vec<&RepoPath> = match section {
        Section::Staged => status.staged().map(|(p, _)| p).collect(),
        Section::Unstaged => status.unstaged().map(|(p, _)| p).collect(),
    };
    entries
        .into_iter()
        .filter(|path| match dir {
            Some(dir) => path.0.starts_with(&format!("{dir}/")),
            None => true,
        })
        .cloned()
        .collect()
}

impl RepoView {
    fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        self.page = page;
        if page == Page::History {
            self.history
                .update(cx, |history, cx| history.ensure_started(cx));
        }
        cx.notify();
    }

    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let diff = cx.new(|_| DiffView::new());
        let history = cx.new(|cx| HistoryPane::new(session.clone(), cx));
        let commit = cx.new(|cx| CommitBox::new(session.clone(), cx));
        let branches = cx.new(|cx| BranchPanel::new(session.clone(), cx));
        cx.observe(&session, |this, _, cx| this.session_changed(cx))
            .detach();
        cx.subscribe(&diff, |this, _, event: &DiffEvent, cx| {
            this.diff_event(event, cx)
        })
        .detach();
        Self {
            session,
            page: Page::Changes,
            history,
            commit,
            branches,
            diff,
            selected: None,
            collapsed: HashSet::new(),
            queue: VecDeque::new(),
            confirm: None,
            bulk: Bulk::Idle,
            local_notice: None,
            last_read: None,
            request: 0,
        }
    }

    fn status(&self, cx: &Context<Self>) -> Option<StatusSnapshot> {
        match &self.session.read(cx).state.snapshot {
            Snapshot::Read { disk, .. } => Some(disk.status.clone()),
            Snapshot::NotRead => None,
        }
    }

    /// Nothing running or waiting, and the newest read has landed.
    fn settled(&self, cx: &Context<Self>) -> bool {
        let state = &self.session.read(cx).state;
        state.ops.running.is_none() && state.ops.queued.is_empty() && state.fresh_disk().is_some()
    }

    /// The session moved. Only a new read can change the list or the diff.
    fn session_changed(&mut self, cx: &mut Context<Self>) {
        if matches!(self.bulk, Bulk::Running) && self.settled(cx) {
            self.bulk = Bulk::Idle;
        }
        self.next_in_queue(cx);
        let Snapshot::Read { read, disk } = &self.session.read(cx).state.snapshot else {
            cx.notify();
            return;
        };
        let (read, status) = (*read, disk.status.clone());
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
                let first = [Section::Unstaged, Section::Staged]
                    .into_iter()
                    .find_map(|section| {
                        files_of(&status, section, None)
                            .into_iter()
                            .next()
                            .map(|path| (section, path))
                    });
                if let Some((section, path)) = first {
                    self.select(section, path, cx);
                }
            }
        }
        cx.notify();
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        self.selected = None;
        self.request += 1;
        self.diff.update(cx, |diff, cx| {
            diff.set_content(DiffContent::Message("No file selected".into()), cx)
        });
    }

    fn select(&mut self, section: Section, path: RepoPath, cx: &mut Context<Self>) {
        self.confirm = None;
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
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { diff_load::load(&paths, &path, section) }
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

    /// A hunk or a set of lines was chosen in the diff.
    fn diff_event(&mut self, event: &DiffEvent, cx: &mut Context<Self>) {
        let Some((section, path)) = self.selected.clone() else {
            return;
        };
        let (part, discard) = match event {
            DiffEvent::Hunk { hunk } => (Part::Hunk(*hunk), false),
            DiffEvent::Lines { hunk, lines } => (
                Part::Lines {
                    hunk: *hunk,
                    lines: lines.clone(),
                },
                false,
            ),
            DiffEvent::DiscardFile => (Part::File, true),
            DiffEvent::DiscardHunk { hunk } => (Part::Hunk(*hunk), true),
            DiffEvent::DiscardLines { hunk, lines } => (
                Part::Lines {
                    hunk: *hunk,
                    lines: lines.clone(),
                },
                true,
            ),
        };
        if discard {
            self.ask_to_discard(&path, &part, cx);
            return;
        }
        let request = {
            let view = self.diff.read(cx);
            let Some(data) = view.data() else { return };
            let Some(inputs) = &data.inputs else { return };
            staging::request(section, &path, &data.diff, inputs, &part)
        };
        if let Some(request) = request {
            self.deliver(request, cx);
        }
    }

    /// Builds the discard of `part` of the file on show from the diff as it
    /// is shown, and asks before sending it.
    fn ask_to_discard(&mut self, path: &RepoPath, part: &Part, cx: &mut Context<Self>) {
        let confirm = {
            let view = self.diff.read(cx);
            let Some(data) = view.data() else { return };
            let Some(inputs) = &data.inputs else { return };
            let Some(event) = staging::discard_request(path, &data.diff, inputs, part) else {
                return;
            };
            let (text, button) = discard::describe(path, &data.diff, inputs, part);
            Confirm {
                text,
                button,
                what: What::Request(Box::new(event)),
            }
        };
        self.local_notice = None;
        self.confirm = Some(confirm);
        cx.notify();
    }

    /// The user said yes.
    fn confirm_discard(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        match confirm.what {
            What::Request(event) => self.deliver(*event, cx),
            What::Files(files) => self.enqueue_with(
                files
                    .into_iter()
                    .map(|path| (Verb::Discard, Section::Unstaged, path))
                    .collect(),
                cx,
            ),
        }
        cx.notify();
    }

    fn cancel_discard(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        cx.notify();
    }

    fn deliver(&mut self, event: Event, cx: &mut Context<Self>) {
        self.local_notice = None;
        self.session
            .update(cx, |session, cx| session.deliver(event, cx));
    }

    /// Something in the tree was clicked.
    fn tree_action(&mut self, action: TreeAction, cx: &mut Context<Self>) {
        let Some(status) = self.status(cx) else {
            return;
        };
        match action {
            TreeAction::Select { section, path } => self.select(section, RepoPath(path), cx),
            TreeAction::Collapse { section, dir } => {
                if !self.collapsed.remove(&(section, dir.clone())) {
                    self.collapsed.insert((section, dir));
                }
                cx.notify();
            }
            TreeAction::ToggleFile { section, path } => self.enqueue(
                files_of(&status, section, None)
                    .into_iter()
                    .filter(|p| p.0 == path)
                    .map(|p| (section, p))
                    .collect(),
                cx,
            ),
            TreeAction::ToggleDir { section, dir } => self.enqueue(
                files_of(&status, section, Some(&dir))
                    .into_iter()
                    .map(|p| (section, p))
                    .collect(),
                cx,
            ),
            TreeAction::DiscardAll => {
                let files = files_of(&status, Section::Unstaged, None);
                if !files.is_empty() {
                    let (text, button) = discard::describe_files(files.len());
                    self.confirm = Some(Confirm {
                        text,
                        button,
                        what: What::Files(files),
                    });
                    self.local_notice = None;
                    cx.notify();
                }
            }
            TreeAction::All { section } => self.enqueue(
                files_of(&status, section, None)
                    .into_iter()
                    .map(|p| (section, p))
                    .collect(),
                cx,
            ),
        }
    }

    fn enqueue(&mut self, files: Vec<(Section, RepoPath)>, cx: &mut Context<Self>) {
        let files = files
            .into_iter()
            .map(|(section, path)| (Verb::Toggle, section, path));
        self.enqueue_with(files.collect(), cx);
    }

    fn enqueue_with(&mut self, files: Vec<(Verb, Section, RepoPath)>, cx: &mut Context<Self>) {
        self.local_notice = None;
        self.queue.extend(files);
        self.next_in_queue(cx);
    }

    /// Starts the next queued file if nothing else is in progress.
    fn next_in_queue(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.bulk, Bulk::Idle) || !self.settled(cx) {
            return;
        }
        let Some((verb, section, path)) = self.queue.pop_front() else {
            return;
        };
        self.bulk = Bulk::Preparing;
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { diff_load::file_request(&paths, &path, section, verb) }
                })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(Some(event)) => {
                    this.bulk = Bulk::Running;
                    this.session
                        .update(cx, |session, cx| session.deliver(event, cx));
                }
                Ok(None) => {
                    this.bulk = Bulk::Idle;
                    this.next_in_queue(cx);
                }
                Err(message) => {
                    let what = if verb == Verb::Discard {
                        "discard"
                    } else {
                        "stage"
                    };
                    this.local_notice = Some(format!("Cannot {what} {}: {message}", path.0));
                    this.bulk = Bulk::Idle;
                    this.next_in_queue(cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        self.deliver(Event::UndoRequested, cx);
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.local_notice = None;
        self.session.update(cx, |session, cx| {
            session.deliver(Event::NoticeDismissed, cx)
        });
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
        let rows: Vec<TreeRow> = match &session.state.snapshot {
            Snapshot::Read { disk, .. } => tree_rows(&disk.status, &self.collapsed),
            Snapshot::NotRead => Vec::new(),
        };
        let undo_label = session
            .state
            .undo
            .top()
            .ok()
            .map(|entry| entry.point.label.clone());
        let notice = session.state.notice.as_ref().and_then(notice::message);
        let selected: Option<(Section, SharedString)> = self
            .selected
            .as_ref()
            .map(|(section, path)| (*section, path.0.clone().into()));
        let this = cx.entity();
        let switch = cx.entity();
        let undo = cx.entity();
        let dismiss = cx.entity();
        let page_switch = Segmented::new(
            "repo-page",
            vec!["Changes".into(), "History".into()],
            match self.page {
                Page::Changes => 0,
                Page::History => 1,
            },
            move |index, _, cx| {
                switch.update(cx, |view, cx| {
                    view.show(
                        if index == 0 {
                            Page::Changes
                        } else {
                            Page::History
                        },
                        cx,
                    )
                })
            },
        );
        let mut controls = div()
            .flex()
            .items_center()
            .gap(px(SPACE_2))
            .child(page_switch);
        if let Some(label) = undo_label {
            controls = controls
                .child(
                    Button::new("undo", "Undo")
                        .on_click(move |_, _, cx| undo.update(cx, |view, cx| view.undo(cx))),
                )
                .child(
                    div()
                        .max_w(px(260.))
                        .truncate()
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(label),
                );
        }
        summary = summary.control(controls);

        let mut banners = div().flex().flex_col().flex_none().gap(px(SPACE_2));
        let mut any = false;
        if let Some(confirm) = &self.confirm {
            let (yes, no) = (cx.entity(), cx.entity());
            banners = banners.child(
                Banner::new(BannerKind::Warning, confirm.text.clone())
                    .action(
                        Button::new("confirm-discard", confirm.button.clone())
                            .danger()
                            .on_click(move |_, _, cx| {
                                yes.update(cx, |view, cx| view.confirm_discard(cx))
                            }),
                    )
                    .action(Button::new("cancel-discard", "Cancel").ghost().on_click(
                        move |_, _, cx| no.update(cx, |view, cx| view.cancel_discard(cx)),
                    )),
            );
            any = true;
        }
        if let Some(text) = &self.local_notice {
            banners = banners.child(Banner::new(BannerKind::Warning, text.clone()));
            any = true;
        }
        if let Some(message) = notice {
            let kind = match message.tone {
                Tone::Warning => BannerKind::Warning,
                Tone::Error => BannerKind::Error,
            };
            let mut banner = Banner::new(kind, message.text).action(
                Button::new("dismiss-notice", "Dismiss")
                    .ghost()
                    .on_click(move |_, _, cx| dismiss.update(cx, |view, cx| view.dismiss(cx))),
            );
            if let Some(details) = message.details {
                banner = banner.action(
                    div()
                        .max_w(px(360.))
                        .truncate()
                        .text_style(&TEXT_SMALL, cx)
                        .child(details.lines().next().unwrap_or_default().to_owned()),
                );
            }
            banners = banners.child(banner);
            any = true;
        }
        let banners = any.then(|| {
            banners
                .p(px(SPACE_2))
                .border_b_1()
                .border_color(hsla(p.line))
        });

        let body = match self.page {
            Page::History => div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(div().flex_1().min_w(px(0.)).child(self.history.clone())),
            Page::Changes => div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_none()
                        .w(px(CHANGES_W))
                        .py(px(SPACE_2))
                        .bg(hsla(p.bg_panel))
                        .border_r_1()
                        .border_color(hsla(p.line))
                        .child(StatusTree::new(rows, selected, move |action, _, cx| {
                            this.update(cx, |this, cx| this.tree_action(action, cx))
                        }))
                        .child(self.commit.clone()),
                )
                .child(div().flex_1().min_w(px(0.)).child(self.diff.clone())),
        };
        let sidebar = div()
            .flex_none()
            .w(px(SIDEBAR))
            .border_r_1()
            .border_color(hsla(p.line))
            .child(self.branches.clone());
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(summary)
            .children(banners)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(sidebar)
                    .child(body),
            )
    }
}
