//! What one repo tab shows: the summary header with the Changes and History
//! switch, then either the status tree beside the diff of the selected file
//! (with staging of files, hunks and lines) or the commit graph. The state it
//! renders belongs to the `RepoSession`; this view remembers which file is
//! selected, which folders are folded, and the files still to stage.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use gpui::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, Styled, Window, deferred, div, px,
};
use invoke_core::{
    BranchName, Event, FileChange, Notice, Phase, PullMode, ReadId, RebasePause, RebaseState,
    RepoPath, Snapshot, StatusSnapshot,
};
use tokens::{
    DIALOG, RADIUS_LG, ROW_DIFF, ROW_LIST, SIDEBAR, SPACE_1, SPACE_2, SPACE_3, SPACE_4, SPACE_6,
    SPACE_8, TEXT_CODE_SMALL, TEXT_SMALL,
};
use ui::convert::{hsla, shadows};
use ui::{
    Banner, BannerKind, Button, DiffContent, DiffEvent, DiffView, Divider, IconName, NavTab,
    NavTabs, RepoSummary, ResizeEdge, ResizeHandle, Section, StatusTree, Summary, TextStyleExt,
    ThemeExt, TreeAction, TreeRow, diff_view, tree_rows,
};

use crate::branch_panel::{BranchPanel, BranchPanelEvent};
use crate::command_palette::{PaletteAction, PaletteItem};
use crate::commit_box::CommitBox;
use crate::commit_pane::{InspectFile, StartRebase};
use crate::conflict_view::ConflictPane;
use crate::diff_load::{self, Loaded};
use crate::discard::{self, Confirm, What};
use crate::file_insights::{FileInsights, FileInsightsEvent, FileMode, FileVersion};
use crate::history_view::{HistoryPane, SelectionChanged};
use crate::notice::{self, Tone};
use crate::pr_view::PrPane;
use crate::rebase_view::{RebaseView, RebaseViewEvent, origin_label, paused_heading};
use crate::reflog_pane::ReflogPane;
use crate::review_view::{ReviewEvent, ReviewPane};
use crate::session::RepoSession;
use crate::staging::{self, Part};
use crate::sync;
use crate::sync_view::{SyncButtons, SyncEvent, SyncStrip};

/// Resizable sidebar bounds from `docs/design/tokens.json` (`200..=360px`).
const SIDEBAR_MIN: f32 = SIDEBAR - SPACE_8 - SPACE_2;
const SIDEBAR_MAX: f32 = SIDEBAR + 3.0 * SPACE_8 + SPACE_6;

/// The changed-file tree is a little wider than the repository sidebar.
const CHANGES_W: f32 = SIDEBAR + 2.0 * SPACE_8;
const CHANGES_MIN: f32 = SIDEBAR - SPACE_4;
const CHANGES_MAX: f32 = DIALOG;

/// Default height of the top History graph pane in the unified Repository workspace.
const GRAPH_H: f32 = SIDEBAR + SPACE_8;
const GRAPH_MIN: f32 = SIDEBAR - SPACE_8 * 3.0;
const GRAPH_MAX: f32 = DIALOG;

/// What the bottom inspector of the unified Repository workspace shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkspaceFocus {
    WorkingTree,
    Commit,
}

/// The pages of a repo tab. Changes and History are combined in `Workspace`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    /// Unified Changes + History workspace.
    Workspace,
    Reflog,
    /// Pull requests (GitHub) & merge requests (GitLab) (P9).
    PullRequests,
    /// The pre-push review, opened by the Push button.
    Review,
    /// The resolver of a paused merge, rebase, cherry-pick or revert (task 23).
    Conflicts,
    /// Interactive rebase editor and paused-rebase controls (P4b).
    Rebase,
    FileInsights,
}

/// Files to stage or unstage one after another. Each stage is built from a
/// fresh read and carries the index id it expects, so the next one waits for
/// the previous to finish and for the repo to be read again.
enum Bulk {
    Idle,
    /// A request was handed to the core; waiting for the session to settle.
    Running,
}

enum QueuedFile {
    Toggle(Section, RepoPath),
    Discard(Box<Event>),
}

/// Events emitted upward from a repo tab to `Workspace` (P10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoViewEvent {
    OpenRepo(PathBuf),
    OpenPalette,
}

pub struct RepoView {
    pub session: Entity<RepoSession>,
    page: Page,
    workspace_focus: WorkspaceFocus,
    graph_collapsed: bool,
    tools_menu_open: bool,
    tab_visible: bool,
    history: Entity<HistoryPane>,
    reflog: Entity<ReflogPane>,
    pull_requests: Entity<PrPane>,
    file_insights: Entity<FileInsights>,
    commit: Entity<CommitBox>,
    branches: Entity<BranchPanel>,
    sync_buttons: Entity<SyncButtons>,
    sync_strip: Entity<SyncStrip>,
    review: Entity<ReviewPane>,
    conflicts: Entity<ConflictPane>,
    rebase: Entity<RebaseView>,
    /// An operation was paused the last time the session moved.
    was_paused: bool,
    /// The paused operation was an interactive rebase `edit` pause (no conflicts).
    was_rebase_edit: bool,
    diff: Entity<DiffView>,
    selected: Option<(Section, RepoPath)>,
    collapsed: HashSet<(Section, String)>,
    queue: VecDeque<QueuedFile>,
    /// A discard waiting for the user's yes, or being prepared for it.
    /// Not tied to `request`, which every watcher read bumps.
    confirm: discard::Confirmation,
    bulk: Bulk,
    /// Something the app itself has to say (a file that cannot be staged).
    local_notice: Option<String>,
    /// The read the list was last reconciled with.
    last_read: Option<ReadId>,
    /// Numbers each diff request; an answer for an older one is dropped.
    request: u64,
    sidebar_w: f32,
    changes_w: f32,
    graph_h: f32,
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

impl EventEmitter<RepoViewEvent> for RepoView {}

impl RepoView {
    fn show(&mut self, page: Page, cx: &mut Context<Self>) {
        self.tools_menu_open = false;
        if self.page == Page::Review && page != Page::Review {
            self.review.update(cx, |review, _| review.close());
        }
        if self.page == Page::PullRequests && page != Page::PullRequests {
            self.pull_requests.update(cx, |prs, _| prs.close());
        }
        if self.page == Page::FileInsights && page != Page::FileInsights {
            self.file_insights
                .update(cx, |insights, _| insights.close());
        }
        self.page = page;
        self.sync_visibility(cx);
        if page == Page::Review {
            self.review.update(cx, |review, cx| review.open(cx));
        }
        if page == Page::PullRequests {
            self.pull_requests.update(cx, |prs, cx| prs.open(cx));
        }
        if page == Page::Workspace {
            self.history
                .update(cx, |history, cx| history.ensure_started(cx));
        }
        cx.notify();
    }

    pub fn set_tab_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.tab_visible = visible;
        self.sync_visibility(cx);
        if visible && self.page == Page::Workspace {
            self.history
                .update(cx, |history, cx| history.ensure_started(cx));
        }
    }

    fn sync_visibility(&self, cx: &mut Context<Self>) {
        let history_visible = self.tab_visible && self.page == Page::Workspace;
        self.history
            .update(cx, |history, cx| history.set_visible(history_visible, cx));
        let reflog_visible = self.tab_visible && self.page == Page::Reflog;
        self.reflog
            .update(cx, |reflog, cx| reflog.set_visible(reflog_visible, cx));
    }

    pub fn open_working_tree(&mut self, cx: &mut Context<Self>) {
        self.workspace_focus = WorkspaceFocus::WorkingTree;
        self.history
            .update(cx, |history, cx| history.select_working_tree(cx));
        self.show(Page::Workspace, cx);
    }

    pub fn open_history(&mut self, cx: &mut Context<Self>) {
        self.graph_collapsed = false;
        self.show(Page::Workspace, cx);
    }

    pub fn open_pull_requests(&mut self, cx: &mut Context<Self>) {
        self.show(Page::PullRequests, cx);
    }

    pub fn open_reflog(&mut self, cx: &mut Context<Self>) {
        self.show(Page::Reflog, cx);
    }

    pub fn open_review(&mut self, cx: &mut Context<Self>) {
        self.show(Page::Review, cx);
    }

    pub fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page != Page::Workspace {
            self.show(Page::Workspace, cx);
        }
        self.graph_collapsed = false;
        self.history
            .update(cx, |history, cx| history.toggle_search(window, cx));
        cx.notify();
    }

    pub fn open_new_branch(&mut self, cx: &mut Context<Self>) {
        self.branches
            .update(cx, |branches, cx| branches.open_create_branch(cx));
    }

    pub fn open_stash(&mut self, cx: &mut Context<Self>) {
        self.branches
            .update(cx, |branches, cx| branches.open_stash(cx));
    }

    pub fn open_new_tag(&mut self, cx: &mut Context<Self>) {
        self.branches
            .update(cx, |branches, cx| branches.open_create_tag(cx));
    }

    pub fn switch_branch(&mut self, name: String, cx: &mut Context<Self>) {
        self.deliver(Event::BranchSwitchRequested(BranchName(name)), cx);
    }

    pub fn trigger_fetch(&mut self, cx: &mut Context<Self>) {
        if let Some(event) = sync::plan(&self.session.read(cx).state).map(|p| p.fetch()) {
            self.deliver(event, cx);
        }
    }

    pub fn trigger_pull(&mut self, cx: &mut Context<Self>) {
        if let Some(event) =
            sync::plan(&self.session.read(cx).state).and_then(|p| p.pull(PullMode::Merge))
        {
            self.deliver(event, cx);
        }
    }

    pub fn trigger_undo(&mut self, cx: &mut Context<Self>) {
        self.undo(cx);
    }

    /// Context-aware commands for the active repository (branches, views, git ops).
    pub fn palette_items(&self, cx: &gpui::App) -> Vec<PaletteItem> {
        let mut items = vec![
            PaletteItem {
                title: "Working tree & changes".into(),
                subtitle: Some("View".into()),
                shortcut: None,
                icon: IconName::Changes,
                action: PaletteAction::ShowWorkingTree,
            },
            PaletteItem {
                title: "Commit history".into(),
                subtitle: Some("View".into()),
                shortcut: None,
                icon: IconName::History,
                action: PaletteAction::ShowHistory,
            },
            PaletteItem {
                title: "Search commits".into(),
                subtitle: Some("History".into()),
                shortcut: Some("Ctrl+F".into()),
                icon: IconName::Search,
                action: PaletteAction::ToggleSearch,
            },
            PaletteItem {
                title: "Pull requests".into(),
                subtitle: Some("View".into()),
                shortcut: None,
                icon: IconName::PullRequest,
                action: PaletteAction::ShowPullRequests,
            },
            PaletteItem {
                title: "Reflog (recovery)".into(),
                subtitle: Some("Tools".into()),
                shortcut: None,
                icon: IconName::History,
                action: PaletteAction::ShowReflog,
            },
            PaletteItem {
                title: "Fetch".into(),
                subtitle: Some("Git".into()),
                shortcut: None,
                icon: IconName::Fetch,
                action: PaletteAction::Fetch,
            },
            PaletteItem {
                title: "Pull".into(),
                subtitle: Some("Git".into()),
                shortcut: None,
                icon: IconName::Pull,
                action: PaletteAction::Pull,
            },
            PaletteItem {
                title: "Review & push".into(),
                subtitle: Some("Git".into()),
                shortcut: None,
                icon: IconName::Push,
                action: PaletteAction::ReviewAndPush,
            },
            PaletteItem {
                title: "New branch…".into(),
                subtitle: Some("Branch".into()),
                shortcut: None,
                icon: IconName::Branch,
                action: PaletteAction::NewBranch,
            },
            PaletteItem {
                title: "Stash changes…".into(),
                subtitle: Some("Git".into()),
                shortcut: None,
                icon: IconName::Stash,
                action: PaletteAction::StashChanges,
            },
            PaletteItem {
                title: "New tag…".into(),
                subtitle: Some("Tag".into()),
                shortcut: None,
                icon: IconName::Tag,
                action: PaletteAction::NewTag,
            },
        ];
        let session = self.session.read(cx);
        if let Ok(entry) = session.state.undo.top() {
            items.push(PaletteItem {
                title: format!("Undo: {}", entry.point.label).into(),
                subtitle: Some("Git".into()),
                shortcut: None,
                icon: IconName::Undo,
                action: PaletteAction::Undo,
            });
        }
        if let Snapshot::Read { disk, .. } = &session.state.snapshot {
            let current = match &disk.head {
                invoke_core::Head::Branch { name, .. } | invoke_core::Head::Unborn(name) => {
                    Some(name.0.as_str())
                }
                invoke_core::Head::Detached(_) => None,
            };
            for ref_name in disk.refs.refs.keys() {
                if let Some(branch) = ref_name.0.strip_prefix("refs/heads/")
                    && Some(branch) != current
                {
                    items.push(PaletteItem {
                        title: format!("Switch branch: {branch}").into(),
                        subtitle: Some("Branch".into()),
                        shortcut: None,
                        icon: IconName::Branch,
                        action: PaletteAction::SwitchBranch(branch.to_owned()),
                    });
                }
            }
        }
        items
    }

    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let diff = cx.new(|_| DiffView::new().with_file_insights());
        let history = cx.new(|cx| {
            let mut pane = HistoryPane::new(session.clone(), cx);
            pane.set_inline_detail(false, cx);
            pane.select_working_tree(cx);
            pane
        });
        let reflog = cx.new(|cx| ReflogPane::new(session.clone(), cx));
        let pull_requests = cx.new(|cx| PrPane::new(session.clone(), cx));
        let reader = session.read(cx).blame.clone();
        let file_insights = cx.new(|_| FileInsights::new(reader));
        let commit = cx.new(|cx| CommitBox::new(session.clone(), cx));
        let branches = cx.new(|cx| BranchPanel::new(session.clone(), cx));
        let sync_buttons = cx.new(|cx| SyncButtons::new(session.clone(), cx));
        let sync_strip = cx.new(|cx| SyncStrip::new(session.clone(), cx));
        let review = cx.new(|cx| ReviewPane::new(session.clone(), cx));
        let conflicts = cx.new(|cx| ConflictPane::new(session.clone(), cx));
        let rebase = cx.new(|cx| RebaseView::new(session.clone(), cx));
        cx.subscribe(&review, |this, _, event: &ReviewEvent, cx| match event {
            ReviewEvent::Back => this.show(Page::Workspace, cx),
            ReviewEvent::CreatePr => {
                this.page = Page::PullRequests;
                this.review.update(cx, |review, _| review.close());
                this.sync_visibility(cx);
                this.pull_requests.update(cx, |prs, cx| prs.open_create(cx));
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(
            &branches,
            |_, _, event: &BranchPanelEvent, cx| match event {
                BranchPanelEvent::OpenWorktree(path) => {
                    cx.emit(RepoViewEvent::OpenRepo(path.clone()));
                }
            },
        )
        .detach();
        cx.subscribe(
            &rebase,
            |this, _, event: &RebaseViewEvent, cx| match event {
                RebaseViewEvent::BackToHistory => this.show(Page::Workspace, cx),
                RebaseViewEvent::OpenChanges => this.open_working_tree(cx),
                RebaseViewEvent::OpenConflicts => this.show(Page::Conflicts, cx),
            },
        )
        .detach();
        cx.subscribe(
            &sync_buttons,
            |this, _, event: &SyncEvent, cx| match event {
                SyncEvent::Review => this.show(Page::Review, cx),
            },
        )
        .detach();
        cx.observe(&session, |this, _, cx| this.session_changed(cx))
            .detach();
        cx.subscribe(&diff, |this, _, event: &DiffEvent, cx| {
            this.diff_event(event, cx)
        })
        .detach();
        cx.subscribe(&history, |this, _, event: &InspectFile, cx| {
            this.inspect_file(event.0.clone(), event.1, cx);
        })
        .detach();
        cx.subscribe(&history, |this, _, event: &StartRebase, cx| {
            this.rebase
                .update(cx, |rebase, cx| rebase.open(event.0.clone(), cx));
            this.show(Page::Rebase, cx);
        })
        .detach();
        cx.subscribe(&history, |this, _, event: &SelectionChanged, cx| {
            this.workspace_focus = match event.0 {
                Some(_) => WorkspaceFocus::Commit,
                None => WorkspaceFocus::WorkingTree,
            };
            cx.notify();
        })
        .detach();
        cx.subscribe(
            &file_insights,
            |this, _, event: &FileInsightsEvent, cx| match event {
                FileInsightsEvent::Back => {
                    this.show(Page::Workspace, cx);
                }
                FileInsightsEvent::Commit(id) => {
                    this.workspace_focus = WorkspaceFocus::Commit;
                    this.show(Page::Workspace, cx);
                    this.history
                        .update(cx, |history, cx| history.open_commit(*id, cx));
                }
            },
        )
        .detach();
        Self {
            session,
            page: Page::Workspace,
            workspace_focus: WorkspaceFocus::WorkingTree,
            graph_collapsed: false,
            tools_menu_open: false,
            tab_visible: false,
            history,
            reflog,
            pull_requests,
            file_insights,
            commit,
            branches,
            sync_buttons,
            sync_strip,
            review,
            conflicts,
            rebase,
            was_paused: false,
            was_rebase_edit: false,
            diff,
            selected: None,
            collapsed: HashSet::new(),
            queue: VecDeque::new(),
            confirm: discard::Confirmation::default(),
            bulk: Bulk::Idle,
            local_notice: None,
            last_read: None,
            request: 0,
            sidebar_w: SIDEBAR,
            changes_w: CHANGES_W,
            graph_h: GRAPH_H,
        }
    }

    fn inspect_file(&mut self, file: FileVersion, mode: FileMode, cx: &mut Context<Self>) {
        self.file_insights
            .update(cx, |insights, cx| insights.open(file, mode, cx));
        self.show(Page::FileInsights, cx);
    }

    fn inspect_change(&mut self, mode: FileMode, cx: &mut Context<Self>) {
        let Some((section, path)) = self.selected.clone() else {
            return;
        };
        let deleted = self.status(cx).is_some_and(|status| {
            status.entries.iter().any(|entry| {
                entry.path == path
                    && (entry.unstaged == Some(FileChange::Deleted)
                        || (section == Section::Staged
                            && entry.staged == Some(FileChange::Deleted)))
            })
        });
        self.inspect_file(
            FileVersion {
                path: path.0,
                revision: deleted.then(|| "HEAD".to_owned()),
            },
            mode,
            cx,
        );
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
        let (paused, rebase_edit, rebase_done) = {
            let state = &self.session.read(cx).state;
            let paused = matches!(
                &state.snapshot,
                Snapshot::Read { disk, .. } if disk.phase.operation().is_some()
            );
            let rebase_edit = matches!(
                &state.snapshot,
                Snapshot::Read { disk, .. }
                    if matches!(
                        &disk.phase,
                        Phase::Rebasing {
                            pause: RebasePause::Edit,
                            conflicts,
                            ..
                        } if conflicts.unresolved_count() == 0
                    )
            );
            (paused, rebase_edit, state.rebase == RebaseState::Done)
        };
        if paused && !self.was_paused {
            self.was_paused = true;
            self.was_rebase_edit = rebase_edit;
            self.show(
                if rebase_edit {
                    Page::Rebase
                } else {
                    Page::Conflicts
                },
                cx,
            );
        } else if paused && self.was_paused {
            if self.was_rebase_edit != rebase_edit {
                self.was_rebase_edit = rebase_edit;
                self.show(
                    if rebase_edit {
                        Page::Rebase
                    } else {
                        Page::Conflicts
                    },
                    cx,
                );
            }
        } else if !paused && self.was_paused {
            self.was_paused = false;
            self.was_rebase_edit = false;
            if matches!(self.page, Page::Conflicts | Page::Rebase) {
                self.show(Page::Workspace, cx);
            }
        } else if !paused && self.page == Page::Rebase && rebase_done {
            self.show(Page::Workspace, cx);
        }
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
        self.confirm.clear();
        self.selected = None;
        self.request += 1;
        self.diff.update(cx, |diff, cx| {
            diff.set_content(DiffContent::Message("No file selected".into()), cx)
        });
    }

    fn select(&mut self, section: Section, path: RepoPath, cx: &mut Context<Self>) {
        self.confirm.clear();
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
                        Loaded::FileMessage { text, mode } => {
                            this.diff.update(cx, |diff, cx| {
                                diff.set_content(
                                    DiffContent::FileMessage {
                                        text: text.into(),
                                        mode,
                                    },
                                    cx,
                                )
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
            DiffEvent::PullLfs => {
                self.deliver(Event::LfsPullRequested { path: Some(path) }, cx);
                return;
            }
            DiffEvent::Blame => {
                self.inspect_change(FileMode::Blame, cx);
                return;
            }
            DiffEvent::FileHistory => {
                self.inspect_change(FileMode::History, cx);
                return;
            }
        };
        if discard {
            if part == Part::File {
                self.prepare_discard(vec![path], cx);
            } else {
                self.ask_to_discard(&path, &part, cx);
            }
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
        // This confirmation is the newest; a file list still being prepared
        // must not replace it.
        self.local_notice = None;
        self.confirm.show(confirm);
        cx.notify();
    }

    fn prepare_discard(&mut self, files: Vec<RepoPath>, cx: &mut Context<Self>) {
        if files.is_empty() {
            return;
        }
        let paths = self.session.read(cx).discovered.paths.clone();
        let ticket = self.confirm.start();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { discard::prepare(&paths, files) })
                .await;
            this.update(cx, |this, cx| {
                match this.confirm.finish(ticket, result) {
                    None => return,
                    Some(Ok(())) => this.local_notice = None,
                    Some(Err(error)) => this.local_notice = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The user said yes.
    fn confirm_discard(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        match confirm.what {
            What::Request(event) => self.deliver(*event, cx),
            What::Files(files) => {
                self.queue.extend(
                    files
                        .into_iter()
                        .map(|event| QueuedFile::Discard(Box::new(event))),
                );
                self.next_in_queue(cx);
            }
        }
        cx.notify();
    }

    fn cancel_discard(&mut self, cx: &mut Context<Self>) {
        self.confirm.clear();
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
                self.prepare_discard(files_of(&status, Section::Unstaged, None), cx);
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
        self.local_notice = None;
        self.queue.extend(
            files
                .into_iter()
                .map(|(section, path)| QueuedFile::Toggle(section, path)),
        );
        self.next_in_queue(cx);
    }

    /// Starts the next queued file if nothing else is in progress.
    fn next_in_queue(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.bulk, Bulk::Idle) || !self.settled(cx) {
            return;
        }
        let Some(disk) = self.session.read(cx).state.fresh_disk() else {
            return;
        };
        while let Some(file) = self.queue.pop_front() {
            let event = match file {
                QueuedFile::Toggle(section, path) => {
                    if !has(&disk.status, section, &path) {
                        continue;
                    }
                    staging::file_request(section, path, disk.index.clone())
                }
                QueuedFile::Discard(event) => *event,
            };
            self.bulk = Bulk::Running;
            self.deliver(event, cx);
            break;
        }
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
        let repo_path = session.discovered.paths.workdir.display().to_string();
        let summary_state = Summary::of(&session.state);
        let changes_badge = summary_state.badge();
        let changes_summary = summary_state.changes();
        let mut summary = RepoSummary::new(summary_state, repo_path.clone());
        if matches!(self.page, Page::Conflicts | Page::Rebase) {
            summary = summary.without_phase();
        }
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
        // A rejected push is shown by the sync strip, with its next steps.
        let notice = session
            .state
            .notice
            .as_ref()
            .filter(|notice| !matches!(notice, Notice::PushRejected { .. }))
            .and_then(notice::message);
        let selected: Option<(Section, SharedString)> = self
            .selected
            .as_ref()
            .map(|(section, path)| (*section, path.0.clone().into()));
        let this = cx.entity();
        let switch = cx.entity();
        let undo = cx.entity();
        let dismiss = cx.entity();
        let show_rebase_tab = self.page == Page::Rebase
            || self.was_rebase_edit
            || matches!(
                session.state.rebase,
                RebaseState::Editing { .. }
                    | RebaseState::Queued(_)
                    | RebaseState::Running { .. }
                    | RebaseState::Reading { .. }
            );
        let repo_tab = {
            let mut t = NavTab::new("Repository").icon(IconName::Changes);
            if !changes_badge.is_empty() {
                t = t.badge(changes_badge);
            }
            t
        };
        let nav_tabs = vec![
            repo_tab,
            NavTab::new("Pull Requests").icon(IconName::PullRequest),
        ];
        let nav_targets = [Page::Workspace, Page::PullRequests];
        let active_nav = nav_targets
            .iter()
            .position(|page| *page == self.page)
            .unwrap_or(usize::MAX);
        let page_switch = NavTabs::new("repo-nav", nav_tabs, active_nav, move |index, _, cx| {
            if let Some(&target) = nav_targets.get(index) {
                switch.update(cx, |view, cx| view.show(target, cx));
            }
        });

        let toggle_tools = cx.entity();
        let close_tools = cx.entity();
        let open_reflog = cx.entity();
        let open_search = cx.entity();
        let toggle_graph = cx.entity();
        let open_palette = cx.entity();
        let tools_popover = self.tools_menu_open.then(|| {
            deferred(
                div()
                    .id("repo-tools-popover")
                    .occlude()
                    .absolute()
                    .top(px(ROW_LIST + SPACE_1))
                    .right(px(0.))
                    .flex()
                    .flex_col()
                    .min_w(px(SIDEBAR - SPACE_8))
                    .gap(px(SPACE_1 / 2.0))
                    .p(px(SPACE_1))
                    .rounded(px(RADIUS_LG))
                    .border_1()
                    .border_color(hsla(p.line))
                    .bg(hsla(p.bg_raised))
                    .shadow(shadows(p.shadow_overlay))
                    .on_mouse_down_out(move |_, _, cx| {
                        cx.stop_propagation();
                        close_tools.update(cx, |view, cx| {
                            view.tools_menu_open = false;
                            cx.notify();
                        });
                    })
                    .child(
                        Button::new("tools-search", "Search commits")
                            .icon(IconName::Search)
                            .shortcut("Ctrl+F")
                            .ghost()
                            .full_width()
                            .on_click(move |_, window, cx| {
                                open_search.update(cx, |view, cx| {
                                    view.tools_menu_open = false;
                                    view.toggle_search(window, cx);
                                });
                            }),
                    )
                    .child(
                        Button::new(
                            "tools-toggle-graph",
                            if self.graph_collapsed {
                                "Show History pane"
                            } else {
                                "Hide History pane"
                            },
                        )
                        .icon(IconName::PanelTop)
                        .ghost()
                        .full_width()
                        .on_click(move |_, _, cx| {
                            toggle_graph.update(cx, |view, cx| {
                                view.tools_menu_open = false;
                                view.graph_collapsed = !view.graph_collapsed;
                                cx.notify();
                            });
                        }),
                    )
                    .child(div().py(px(SPACE_1 / 2.0)).child(Divider::horizontal()))
                    .child(
                        Button::new("tools-reflog", "Reflog (recovery)")
                            .icon(IconName::History)
                            .ghost()
                            .full_width()
                            .on_click(move |_, _, cx| {
                                open_reflog.update(cx, |view, cx| view.open_reflog(cx));
                            }),
                    )
                    .child(
                        Button::new("tools-palette", "Command palette")
                            .icon(IconName::Command)
                            .shortcut("Ctrl+K")
                            .ghost()
                            .full_width()
                            .on_click(move |_, _, cx| {
                                open_palette.update(cx, |view, cx| {
                                    view.tools_menu_open = false;
                                    cx.emit(RepoViewEvent::OpenPalette);
                                    cx.notify();
                                });
                            }),
                    ),
            )
        });

        let open_conflicts = cx.entity();
        let open_rebase = cx.entity();
        let back_reflog = cx.entity();
        let controls = div()
            .flex()
            .flex_wrap()
            .flex_1()
            .min_w(px(0.))
            .items_center()
            .gap(px(SPACE_2))
            .child(page_switch)
            .children((self.page == Page::Reflog).then(|| {
                Button::new("reflog-back-workspace", "← Back to Repository")
                    .ghost()
                    .compact()
                    .on_click(move |_, _, cx| {
                        back_reflog.update(cx, |view, cx| view.show(Page::Workspace, cx));
                    })
            }))
            .children(
                (self.was_paused && !self.was_rebase_edit && self.page != Page::Conflicts).then(
                    || {
                        Button::new("open-active-conflicts", "Conflicts")
                            .danger()
                            .compact()
                            .on_click(move |_, _, cx| {
                                open_conflicts
                                    .update(cx, |view, cx| view.show(Page::Conflicts, cx));
                            })
                    },
                ),
            )
            .children((show_rebase_tab && self.page != Page::Rebase).then(|| {
                Button::new("open-active-rebase", "Rebase")
                    .compact()
                    .on_click(move |_, _, cx| {
                        open_rebase.update(cx, |view, cx| view.show(Page::Rebase, cx));
                    })
            }))
            .child(div().flex_1())
            .child(self.sync_buttons.clone())
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(
                        Button::new("repo-tools-btn", "")
                            .icon(IconName::More)
                            .ghost()
                            .on_click(move |_, _, cx| {
                                toggle_tools.update(cx, |view, cx| {
                                    view.tools_menu_open = !view.tools_menu_open;
                                    cx.notify();
                                });
                            }),
                    )
                    .children(tools_popover),
            );
        summary = summary.control(controls);

        let mut banners = div().flex().flex_col().flex_none().gap(px(SPACE_2));
        let mut any = false;
        if !matches!(self.page, Page::Conflicts | Page::Rebase)
            && let Some(disk) = session.state.snapshot.disk()
            && let Phase::Rebasing {
                step,
                total,
                conflicts,
                pause,
                ..
            } = &disk.phase
        {
            let branch = origin_label(disk);
            let heading = paused_heading(*step, *total, *pause, &branch);
            let busy = session.state.ops.running.is_some();
            let ready = matches!(
                invoke_core::continue_gate(&disk.phase),
                invoke_core::ContinueGate::Ready
            );
            let (cont, skip, open) = (cx.entity(), cx.entity(), cx.entity());
            let mut banner = Banner::new(BannerKind::Warning, heading);
            if conflicts.unresolved_count() > 0 {
                banner = banner.action(
                    Button::new("banner-open-conflicts", "Open Conflicts")
                        .primary()
                        .on_click(move |_, _, cx| {
                            open.update(cx, |view, cx| view.show(Page::Conflicts, cx))
                        }),
                );
            } else {
                banner = banner
                    .action(
                        Button::new("banner-rebase-continue", "Continue rebase")
                            .primary()
                            .disabled(!ready || busy)
                            .on_click(move |_, _, cx| {
                                cont.update(cx, |view, cx| {
                                    view.deliver(Event::ContinueRequested, cx)
                                })
                            }),
                    )
                    .action(
                        Button::new("banner-rebase-skip", "Skip")
                            .disabled(busy)
                            .on_click(move |_, _, cx| {
                                skip.update(cx, |view, cx| view.deliver(Event::SkipRequested, cx))
                            }),
                    )
                    .action(
                        Button::new("banner-open-rebase", "Rebase")
                            .ghost()
                            .on_click(move |_, _, cx| {
                                open.update(cx, |view, cx| view.show(Page::Rebase, cx))
                            }),
                    );
            }
            banners = banners.child(banner);
            any = true;
        }
        if let Some(confirm) = self.confirm.shown() {
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
                        .child(crate::notice::headline(&details)),
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
            Page::Reflog => div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(self.reflog.clone()),
            Page::PullRequests => div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(self.pull_requests.clone()),
            Page::FileInsights => div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(self.file_insights.clone()),
            Page::Conflicts => div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(self.conflicts.clone()),
            Page::Rebase => div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(self.rebase.clone()),
            Page::Review => div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .child(self.review.clone()),
            Page::Workspace => {
                let resize_changes = cx.entity();
                let resize_graph = cx.entity();
                let bottom_pane = match self.workspace_focus {
                    WorkspaceFocus::WorkingTree => div()
                        .flex()
                        .flex_1()
                        .min_h(px(0.))
                        .min_w(px(0.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_none()
                                .w(px(self.changes_w))
                                .bg(hsla(p.bg_panel))
                                .child(div().flex().flex_1().min_h(px(0.)).pt(px(SPACE_2)).child(
                                    StatusTree::new(rows, selected, move |action, _, cx| {
                                        this.update(cx, |this, cx| this.tree_action(action, cx))
                                    }),
                                ))
                                .child(self.commit.clone()),
                        )
                        .child(ResizeHandle::new(
                            "changes-pane-resize",
                            self.changes_w,
                            CHANGES_W,
                            CHANGES_MIN,
                            CHANGES_MAX,
                            ResizeEdge::Right,
                            move |width, _, cx| {
                                resize_changes.update(cx, |view, cx| {
                                    view.changes_w = width;
                                    cx.notify();
                                });
                            },
                        ))
                        .child(
                            div().flex().flex_col().flex_1().min_w(px(0.)).child(
                                div().flex().flex_1().min_h(px(0.)).child(self.diff.clone()),
                            ),
                        )
                        .into_any_element(),
                    WorkspaceFocus::Commit => div()
                        .flex()
                        .flex_1()
                        .min_h(px(0.))
                        .min_w(px(0.))
                        .child(self.history.read(cx).detail())
                        .into_any_element(),
                };
                let mut stack = div().flex().flex_col().flex_1().min_h(px(0.)).min_w(px(0.));
                if !self.graph_collapsed {
                    stack = stack
                        .child(
                            div()
                                .flex_none()
                                .h(px(self.graph_h))
                                .min_h(px(0.))
                                .w_full()
                                .child(self.history.clone()),
                        )
                        .child(ResizeHandle::new(
                            "workspace-graph-resize",
                            self.graph_h,
                            GRAPH_H,
                            GRAPH_MIN,
                            GRAPH_MAX,
                            ResizeEdge::Bottom,
                            move |height, _, cx| {
                                resize_graph.update(cx, |view, cx| {
                                    view.graph_h = height;
                                    cx.notify();
                                });
                            },
                        ));
                }
                stack.child(bottom_pane)
            }
        }
        .min_w(px(0.));
        let resize_sidebar = cx.entity();
        let sidebar = div()
            .flex_none()
            .w(px(self.sidebar_w))
            .child(self.branches.clone());
        let sidebar_handle = ResizeHandle::new(
            "repo-sidebar-resize",
            self.sidebar_w,
            SIDEBAR,
            SIDEBAR_MIN,
            SIDEBAR_MAX,
            ResizeEdge::Right,
            move |width, _, cx| {
                resize_sidebar.update(cx, |view, cx| {
                    view.sidebar_w = width;
                    cx.notify();
                });
            },
        );
        let mut content = div().flex().flex_1().min_h(px(0.)).min_w(px(0.));
        if !matches!(self.page, Page::Review | Page::Conflicts | Page::Rebase) {
            content = content.child(sidebar).child(sidebar_handle);
        }

        let toggle_graph_bar = cx.entity();
        let status_bar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(SPACE_3))
            .h(px(ROW_DIFF + SPACE_1))
            .px(px(SPACE_3))
            .bg(hsla(p.bg_app))
            .border_t_1()
            .border_color(hsla(p.line))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_style(&TEXT_CODE_SMALL, cx)
                    .text_color(hsla(p.ink_faint))
                    .child(repo_path),
            )
            .child(
                div()
                    .flex_none()
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(changes_summary),
            )
            .children(undo_label.map(|label| {
                Button::new("undo", format!("Undo {label}"))
                    .icon(IconName::Undo)
                    .ghost()
                    .compact()
                    .on_click(move |_, _, cx| undo.update(cx, |view, cx| view.undo(cx)))
            }))
            .children((self.page == Page::Workspace).then(|| {
                Button::new(
                    "status-toggle-history",
                    if self.graph_collapsed {
                        "Show history"
                    } else {
                        "Hide history"
                    },
                )
                .icon(IconName::PanelTop)
                .ghost()
                .compact()
                .on_click(move |_, _, cx| {
                    toggle_graph_bar.update(cx, |view, cx| {
                        view.graph_collapsed = !view.graph_collapsed;
                        cx.notify();
                    });
                })
            }))
            .child(
                div()
                    .flex_none()
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_faint))
                    .child("Ctrl+K Commands · F5 Refresh"),
            );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .child(summary)
            .child(self.sync_strip.clone())
            .children(banners)
            .child(content.child(body))
            .child(status_bar)
    }
}
