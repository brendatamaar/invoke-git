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
use tokens::{SPACE_3, SPACE_4, TEXT_BODY, TEXT_TITLE};
use ui::convert::hsla;
use ui::{Banner, BannerKind, Button, Summary, TabBar, TabItem, TextStyleExt, ThemeExt, Welcome};

use crate::clone_view::{CloneView, CloneViewEvent};
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
        Refresh
    ]
);

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
    notice: Option<Notice>,
    /// The clone dialog, shown in place of the welcome screen.
    cloning: Option<(Entity<CloneView>, Subscription)>,
    pub focus: FocusHandle,
    title: String,
}

impl Workspace {
    pub fn new(restore: Saved, open: Vec<PathBuf>, cx: &mut Context<Self>) -> Self {
        let mut workspace = Self {
            git: Git::detect_with_fallback(),
            tabs: Tabs::new(),
            active_repo: None,
            notice: None,
            cloning: None,
            focus: cx.focus_handle(),
            title: String::new(),
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
        let existing = self.tabs.position(|view| {
            paths::same_repo(
                &view.read(cx).session.read(cx).discovered.paths.workdir,
                &found.paths.workdir,
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
        self.changed(cx);
    }

    fn select_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.tabs.select(index);
        self.changed(cx);
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.tabs.close(index);
        self.changed(cx);
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
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_4))
                    .w(px(520.))
                    .child(
                        div()
                            .text_style(&TEXT_TITLE, cx)
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

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> TabBar {
        let items = self
            .tabs
            .items()
            .iter()
            .map(|view| {
                let session = view.read(cx).session.read(cx);
                TabItem {
                    title: paths::title(&session.discovered.paths.workdir).into(),
                    badge: Summary::of(&session.state).badge().into(),
                }
            })
            .collect();
        let (select, close, new) = (cx.entity(), cx.entity(), cx.entity());
        TabBar::new(
            items,
            self.tabs.active_index(),
            move |index, _, cx| select.update(cx, |this, cx| this.select_tab(index, cx)),
            move |index, _, cx| close.update(cx, |this, cx| this.close_tab(index, cx)),
            move |_, cx| new.update(cx, |this, cx| this.show_welcome(cx)),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let title = self.window_title(cx);
        if title != self.title {
            window.set_window_title(&title);
            self.title = title;
        }

        let root = div()
            .track_focus(&self.focus)
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
        root.child(self.render_tab_bar(cx)).child(
            div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .px(px(SPACE_3 * 0.))
                .child(content),
        )
    }
}
