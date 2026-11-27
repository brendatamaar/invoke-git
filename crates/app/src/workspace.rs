//! The window's root view: repo tabs, the welcome screen, and the setup
//! screen when git is missing. It owns the tab list and the sessions; every
//! repo's own state lives in its `RepoSession`.

use std::path::PathBuf;

use gpui::{
    AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement, ParentElement,
    PathPromptOptions, Render, Styled, Subscription, Window, actions, div, px,
};
use invoke_core::Event;
use shell::{DiscoverError, Discovered, Git, git::SetupError};
use tokens::{DIALOG, RADIUS_LG, SPACE_4, SPACE_6, SPACE_8, TEXT_BODY, TEXT_DISPLAY, Theme};
use ui::convert::{hsla, shadows};
use ui::{
    Banner, BannerKind, Button, IconName, RecentRepo, Summary, TabBar, TabItem, TextStyleExt,
    ThemeExt, Welcome, set_theme,
};

use crate::clone_view::{CloneView, CloneViewEvent};
use crate::command_palette::{CommandPalette, PaletteAction, PaletteEvent, PaletteItem};
use crate::paths;
use crate::persist::{self, Saved};
use crate::repo_view::{RepoView, RepoViewEvent};
use crate::session::RepoSession;
use crate::setup;
use crate::tabs::{Active, Tabs};

actions!(
    invoke_git,
    [
        OpenRepository,
        InitRepository,
        NewTab,
        CloseTab,
        NextTab,
        PreviousTab,
        Refresh,
        FocusNext,
        FocusPrev,
        OpenPalette,
        ToggleSearch
    ]
);

fn display_path(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else {
        text
    }
}

/// What the welcome screen says about the last attempt.
enum Notice {
    /// The folder is not inside a repository: offer `git init` there.
    NotARepo(PathBuf),
    Failed(String),
}

pub struct Workspace {
    git: Result<Git, SetupError>,
    tabs: Tabs<Entity<RepoView>>,
    active_repo: Option<Entity<RepoView>>,
    recent: Vec<PathBuf>,
    notice: Option<Notice>,
    /// The clone dialog, shown in place of the welcome screen.
    cloning: Option<(Entity<CloneView>, Subscription)>,
    palette: Entity<CommandPalette>,
    pending_search: bool,
    pub focus: FocusHandle,
    title: String,
    _palette_sub: Subscription,
}

impl Workspace {
    pub fn new(restore: Saved, open: Vec<PathBuf>, cx: &mut Context<Self>) -> Self {
        let palette = cx.new(CommandPalette::new);
        let palette_sub = cx.subscribe(&palette, |this, _, event: &PaletteEvent, cx| match event {
            PaletteEvent::Run(action) => this.run_palette_action(action.clone(), cx),
            PaletteEvent::Closed => cx.notify(),
        });
        let mut workspace = Self {
            git: Git::detect_with_fallback(),
            tabs: Tabs::new(),
            active_repo: None,
            recent: persist::load_recent(),
            notice: None,
            cloning: None,
            palette,
            pending_search: false,
            focus: cx.focus_handle(),
            title: String::new(),
            _palette_sub: palette_sub,
        };
        workspace.restore(restore, open, cx);
        workspace
    }

    fn retry_setup(&mut self, cx: &mut Context<Self>) {
        self.git = Git::detect_with_fallback();
        cx.notify();
    }

    /// Reopens the tabs of the last run, one after the other so they keep
    /// their order. A folder that is gone is skipped.
    fn restore(&mut self, saved: Saved, open: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.git.is_err() {
            return;
        }
        cx.spawn(async move |this, cx| {
            for path in saved.paths {
                let found = cx
                    .background_executor()
                    .spawn(async move { shell::discover(&path) })
                    .await;
                let Ok(found) = found else { continue };
                if this
                    .update(cx, |this, cx| this.add_repo(found, cx))
                    .is_err()
                {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                match saved.active {
                    Some(index) => this.tabs.select(index),
                    None => this.tabs.show_welcome(),
                }
                this.changed(cx);
                // Folders named on the command line report their own errors.
                for path in open {
                    this.open_path(path, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn saved(&self, cx: &gpui::App) -> Saved {
        Saved {
            paths: self
                .tabs
                .items()
                .iter()
                .map(|view| {
                    view.read(cx)
                        .session
                        .read(cx)
                        .discovered
                        .paths
                        .workdir
                        .clone()
                })
                .collect(),
            active: self.tabs.active_index(),
        }
    }

    /// Remembers the tabs and repaints.
    fn changed(&mut self, cx: &mut Context<Self>) {
        let active = self.tabs.active_item().cloned();
        if self.active_repo != active {
            if let Some(previous) = self.active_repo.take() {
                previous.update(cx, |repo, cx| repo.set_tab_visible(false, cx));
            }
            self.active_repo = active;
            if let Some(active) = &self.active_repo {
                active.update(cx, |repo, cx| repo.set_tab_visible(true, cx));
            }
        }
        persist::save(&self.saved(cx));
        cx.notify();
    }

    /// Shows the folder picker and calls `then` with the chosen folder.
    fn choose_folder(
        &mut self,
        prompt: &'static str,
        then: fn(&mut Self, PathBuf, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(prompt.into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| then(this, path, cx)).ok();
            }
        })
        .detach();
    }

    pub fn choose_open(&mut self, cx: &mut Context<Self>) {
        self.choose_folder("Open repository", Self::open_path, cx);
    }

    pub fn choose_init(&mut self, cx: &mut Context<Self>) {
        self.choose_folder("Initialize repository", Self::init_path, cx);
    }

    /// Finds the repo that contains `path` and opens it in a tab.
    pub fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { shell::discover(&path) }
                })
                .await;
            this.update(cx, |this, cx| match found {
                Ok(found) => this.add_repo(found, cx),
                Err(DiscoverError::NotARepo { .. }) => {
                    this.notice = Some(Notice::NotARepo(path));
                    this.tabs.show_welcome();
                    this.changed(cx);
                }
                Err(error) => {
                    this.notice = Some(Notice::Failed(error.to_string()));
                    this.tabs.show_welcome();
                    this.changed(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Runs `git init` in `path`, then opens it.
    pub fn init_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Ok(git) = self.git.clone() else { return };
        cx.spawn(async move |this, cx| {
            let output = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { git.command(&path).arg("init").output() }
                })
                .await;
            this.update(cx, |this, cx| match output {
                Ok(output) if output.status.success() => this.open_path(path, cx),
                Ok(output) => {
                    let text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                    this.notice = Some(Notice::Failed(format!(
                        "git init failed in {}: {text}",
                        path.display()
                    )));
                    this.changed(cx);
                }
                Err(error) => {
                    this.notice = Some(Notice::Failed(format!(
                        "Could not run git init in {}: {error}",
                        path.display()
                    )));
                    this.changed(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Opens a discovered repo in a new tab, or selects its tab if it is
    /// already open (the same folder in any spelling).
    fn add_repo(&mut self, found: Discovered, cx: &mut Context<Self>) {
        let Ok(git) = self.git.clone() else { return };
        self.notice = None;
        let workdir = found.paths.workdir.clone();
        let existing = self.tabs.position(|view| {
            paths::same_repo(
                &view.read(cx).session.read(cx).discovered.paths.workdir,
                &workdir,
            )
        });
        self.tabs.open(existing, || {
            let session = cx.new(|cx| RepoSession::new(found, git, cx));
            cx.observe(&session, |_, _, cx| cx.notify()).detach();
            let view = cx.new(|cx| RepoView::new(session, cx));
            cx.subscribe(&view, |this, _, event: &RepoViewEvent, cx| match event {
                RepoViewEvent::OpenRepo(path) => this.open_path(path.clone(), cx),
            })
            .detach();
            view
        });
        persist::record_recent(&mut self.recent, workdir);
        persist::save_recent(&self.recent);
        self.changed(cx);
    }

    fn select_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.tabs.select(index);
        self.changed(cx);
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(view) = self.tabs.items().get(index) {
            let workdir = view
                .read(cx)
                .session
                .read(cx)
                .discovered
                .paths
                .workdir
                .clone();
            persist::record_recent(&mut self.recent, workdir);
            persist::save_recent(&self.recent);
        }
        self.tabs.close(index);
        self.changed(cx);
    }

    fn open_recent(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(path) = self.recent.get(index).cloned() {
            self.open_path(path, cx);
        }
    }

    fn remove_recent(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(path) = self.recent.get(index).cloned() {
            persist::remove_recent(&mut self.recent, &path);
            persist::save_recent(&self.recent);
            cx.notify();
        }
    }

    fn clear_recent(&mut self, cx: &mut Context<Self>) {
        if !self.recent.is_empty() {
            self.recent.clear();
            persist::save_recent(&self.recent);
            cx.notify();
        }
    }

    /// Reads the selected repo again (F5), for when the watcher is not running.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self
            .tabs
            .active_item()
            .map(|view| view.read(cx).session.clone())
        {
            session.update(cx, |session, cx| {
                session.deliver(Event::RefreshRequested, cx)
            });
        }
    }

    /// Opens the clone dialog.
    fn start_clone(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(git) = self.git.clone() else { return };
        let view = cx.new(|cx| CloneView::new(git, cx));
        view.update(cx, |view, cx| view.focus(window, cx));
        let subscription = cx.subscribe(&view, |this, _, event: &CloneViewEvent, cx| {
            this.cloning = None;
            if let CloneViewEvent::Cloned(path) = event {
                this.open_path(path.clone(), cx);
            }
            cx.notify();
        });
        self.cloning = Some((view, subscription));
        self.tabs.show_welcome();
        self.changed(cx);
    }

    fn show_welcome(&mut self, cx: &mut Context<Self>) {
        self.tabs.show_welcome();
        self.changed(cx);
    }

    fn close_active(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.active_index() {
            self.close_tab(index, cx);
        }
    }

    fn select_next_tab(&mut self, cx: &mut Context<Self>) {
        self.tabs.select_next();
        self.changed(cx);
    }

    fn select_previous_tab(&mut self, cx: &mut Context<Self>) {
        self.tabs.select_previous();
        self.changed(cx);
    }

    /// Routes the global shortcuts to this workspace. They are global, not tied
    /// to a focused element: a click on a button that then disappears (for
    /// example "Initialize here") must not leave the keyboard dead.
    pub fn register_actions(this: &Entity<Self>, cx: &mut gpui::App) {
        macro_rules! route {
            ($action:ty => $method:ident) => {{
                let this = this.downgrade();
                cx.on_action(move |_: &$action, cx| {
                    this.update(cx, |this, cx| this.$method(cx)).ok();
                });
            }};
        }
        route!(OpenRepository => choose_open);
        route!(InitRepository => choose_init);
        route!(NewTab => show_welcome);
        route!(CloseTab => close_active);
        route!(Refresh => refresh);
        route!(NextTab => select_next_tab);
        route!(PreviousTab => select_previous_tab);
    }

    fn window_title(&self, cx: &gpui::App) -> String {
        match self.tabs.active_item() {
            Some(view) => format!(
                "{} — Invoke Git",
                paths::title(&view.read(cx).session.read(cx).discovered.paths.workdir)
            ),
            None => "Invoke Git".to_owned(),
        }
    }

    fn render_setup(&self, error: &SetupError, cx: &mut Context<Self>) -> gpui::AnyElement {
        let p = cx.palette();
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .p(px(SPACE_8))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_4))
                    .w(px(DIALOG))
                    .p(px(SPACE_6))
                    .rounded(px(RADIUS_LG))
                    .bg(hsla(p.bg_panel))
                    .border_1()
                    .border_color(hsla(p.line))
                    .shadow(shadows(p.shadow_overlay))
                    .child(
                        div()
                            .text_style(&TEXT_DISPLAY, cx)
                            .text_color(hsla(p.ink))
                            .child(setup::headline()),
                    )
                    .child(
                        div()
                            .text_style(&TEXT_BODY, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(setup::message(error)),
                    )
                    .child(
                        div().flex().child(
                            Button::new("setup-retry", "Check again")
                                .primary()
                                .on_click(cx.listener(|this, _, _, cx| this.retry_setup(cx))),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some((view, _)) = &self.cloning {
            return view.clone().into_any_element();
        }
        let mut welcome = Welcome::new(
            {
                let this = cx.entity();
                move |_, cx| this.update(cx, |this, cx| this.choose_open(cx))
            },
            {
                let this = cx.entity();
                move |_, cx| this.update(cx, |this, cx| this.choose_init(cx))
            },
        )
        .on_clone({
            let this = cx.entity();
            move |window, cx| this.update(cx, |this, cx| this.start_clone(window, cx))
        });
        if !self.recent.is_empty() {
            let items = self
                .recent
                .iter()
                .map(|path| RecentRepo {
                    title: paths::title(path).into(),
                    path: display_path(path).into(),
                })
                .collect();
            let (open_ent, remove_ent, clear_ent) = (cx.entity(), cx.entity(), cx.entity());
            welcome = welcome.recent(
                items,
                move |index, _, cx| open_ent.update(cx, |this, cx| this.open_recent(index, cx)),
                move |index, _, cx| remove_ent.update(cx, |this, cx| this.remove_recent(index, cx)),
                move |_, cx| clear_ent.update(cx, |this, cx| this.clear_recent(cx)),
            );
        }
        match &self.notice {
            Some(Notice::NotARepo(path)) => {
                let target = path.clone();
                welcome = welcome.notice(
                    Banner::new(
                        BannerKind::Warning,
                        format!("{} is not inside a git repository.", paths::title(path)),
                    )
                    .action(
                        Button::new("init-here", "Initialize here").on_click(
                            cx.listener(move |this, _, _, cx| this.init_path(target.clone(), cx)),
                        ),
                    ),
                );
            }
            Some(Notice::Failed(message)) => {
                welcome = welcome.notice(Banner::new(BannerKind::Error, message.clone()));
            }
            None => {}
        }
        welcome.into_any_element()
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.read(cx).is_open() {
            self.palette.update(cx, |palette, cx| palette.close(cx));
            return;
        }
        let items = self.build_palette_items(cx);
        self.palette
            .update(cx, |palette, cx| palette.open(items, window, cx));
    }

    fn open_commit_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(repo) = self.tabs.active_item().cloned() {
            repo.update(cx, |view, cx| view.toggle_search(window, cx));
        }
    }

    fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(repo) = self.tabs.active_item().cloned() {
            repo.update(cx, |view, cx| view.find_in_view(window, cx));
        }
    }

    fn build_palette_items(&self, cx: &gpui::App) -> Vec<PaletteItem> {
        let mut items = Vec::new();
        if let Some(repo) = self.tabs.active_item() {
            items.extend(repo.read(cx).palette_items(cx));
        }
        items.extend([
            PaletteItem {
                title: "Refresh repository".into(),
                subtitle: Some("Git".into()),
                shortcut: Some("F5".into()),
                icon: IconName::Refresh,
                action: PaletteAction::Refresh,
            },
            PaletteItem {
                title: "Open repository…".into(),
                subtitle: Some("Workspace".into()),
                shortcut: Some("Ctrl+O".into()),
                icon: IconName::Worktree,
                action: PaletteAction::OpenRepo,
            },
            PaletteItem {
                title: "Initialize repository…".into(),
                subtitle: Some("Workspace".into()),
                shortcut: Some("Ctrl+Shift+O".into()),
                icon: IconName::Plus,
                action: PaletteAction::InitRepo,
            },
            PaletteItem {
                title: "Clone repository…".into(),
                subtitle: Some("Workspace".into()),
                shortcut: None,
                icon: IconName::Fetch,
                action: PaletteAction::CloneRepo,
            },
            PaletteItem {
                title: "Toggle Light / Dark theme".into(),
                subtitle: Some("Appearance".into()),
                shortcut: None,
                icon: match cx.ui_theme().theme() {
                    Theme::Dark => IconName::Sun,
                    Theme::Light => IconName::Moon,
                },
                action: PaletteAction::ToggleTheme,
            },
        ]);
        for path in &self.recent {
            items.push(PaletteItem {
                title: format!("Open recent: {}", paths::title(path)).into(),
                subtitle: Some(display_path(path).into()),
                shortcut: None,
                icon: IconName::Worktree,
                action: PaletteAction::OpenRecent(path.clone()),
            });
        }
        items
    }

    fn run_palette_action(&mut self, action: PaletteAction, cx: &mut Context<Self>) {
        let active = self.tabs.active_item().cloned();
        match action {
            PaletteAction::OpenRepo => self.choose_open(cx),
            PaletteAction::InitRepo => self.choose_init(cx),
            PaletteAction::CloneRepo => {
                if let Ok(git) = self.git.clone() {
                    let view = cx.new(|cx| CloneView::new(git, cx));
                    let subscription =
                        cx.subscribe(&view, |this, _, event: &CloneViewEvent, cx| {
                            this.cloning = None;
                            if let CloneViewEvent::Cloned(path) = event {
                                this.open_path(path.clone(), cx);
                            }
                            cx.notify();
                        });
                    self.cloning = Some((view, subscription));
                    self.tabs.show_welcome();
                    self.changed(cx);
                }
            }
            PaletteAction::OpenRecent(path) => self.open_path(path, cx),
            PaletteAction::ShowWorkingTree => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.open_working_tree(cx));
                }
            }
            PaletteAction::ShowHistory => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.open_history(cx));
                }
            }
            PaletteAction::ShowPullRequests => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.open_pull_requests(cx));
                }
            }
            PaletteAction::ShowReflog => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.open_reflog(cx));
                }
            }
            PaletteAction::ToggleSearch => {
                self.pending_search = true;
                cx.notify();
            }
            PaletteAction::SwitchBranch(name) => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.switch_branch(name, cx));
                }
            }
            PaletteAction::NewBranch => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.open_new_branch(cx));
                }
            }
            PaletteAction::StashChanges => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.open_stash(cx));
                }
            }
            PaletteAction::NewTag => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.open_new_tag(cx));
                }
            }
            PaletteAction::Fetch => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.trigger_fetch(cx));
                }
            }
            PaletteAction::Pull => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.trigger_pull(cx));
                }
            }
            PaletteAction::ReviewAndPush => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.open_review(cx));
                }
            }
            PaletteAction::Refresh => self.refresh(cx),
            PaletteAction::Undo => {
                if let Some(repo) = active {
                    repo.update(cx, |view, cx| view.trigger_undo(cx));
                }
            }
            PaletteAction::ToggleTheme => {
                let next = match cx.ui_theme().theme() {
                    Theme::Dark => Theme::Light,
                    Theme::Light => Theme::Dark,
                };
                set_theme(next, cx);
            }
        }
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> TabBar {
        let repositories: Vec<_> = self
            .tabs
            .items()
            .iter()
            .map(|view| &view.read(cx).session.read(cx).discovered)
            .collect();
        let items = self
            .tabs
            .items()
            .iter()
            .zip(&repositories)
            .map(|(view, repo)| {
                let session = view.read(cx).session.read(cx);
                let workdir = &repo.paths.workdir;
                let name = paths::title(workdir);
                let same_name = repositories
                    .iter()
                    .filter(|other| paths::title(&other.paths.workdir).eq_ignore_ascii_case(&name))
                    .count()
                    > 1;
                let mut title = if same_name {
                    format!(
                        "{name} · {}",
                        workdir
                            .parent()
                            .map_or_else(|| display_path(workdir), display_path)
                    )
                } else {
                    name
                };
                if repo.paths.git_dir != repo.common_dir {
                    title.push_str(" · Worktree");
                }
                TabItem {
                    title: title.into(),
                    path: display_path(workdir).into(),
                    badge: Summary::of(&session.state).badge().into(),
                }
            })
            .collect();
        let (select, close, new, palette) = (cx.entity(), cx.entity(), cx.entity(), cx.entity());
        TabBar::new(
            items,
            self.tabs.active_index(),
            move |index, _, cx| select.update(cx, |this, cx| this.select_tab(index, cx)),
            move |index, _, cx| close.update(cx, |this, cx| this.close_tab(index, cx)),
            move |_, cx| new.update(cx, |this, cx| this.show_welcome(cx)),
        )
        .on_palette(move |window, cx| palette.update(cx, |this, cx| this.open_palette(window, cx)))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.pending_search) {
            self.open_commit_search(window, cx);
        }
        let p = cx.palette();
        let title = self.window_title(cx);
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }

        let root = div()
            .track_focus(&self.focus)
            .on_action(|_: &FocusNext, window, cx| window.focus_next(cx))
            .on_action(|_: &FocusPrev, window, cx| window.focus_prev(cx))
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                this.open_palette(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleSearch, window, cx| {
                this.toggle_search(window, cx);
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(hsla(p.bg_app))
            .text_color(hsla(p.ink))
            .text_style(&TEXT_BODY, cx);

        if let Err(error) = &self.git {
            return root.child(self.render_setup(error, cx));
        }
        let content = match self.tabs.active() {
            Active::Welcome => self.render_welcome(cx),
            Active::Repo(index) => self.tabs.items()[index].clone().into_any_element(),
        };
        root.child(self.render_tab_bar(cx))
            .child(div().flex().flex_1().min_h(px(0.)).child(content))
            .child(self.palette.clone())
    }
}
