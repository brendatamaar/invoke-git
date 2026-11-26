//! The clone dialog: address, folder, progress, cancel and the credential
//! prompt. The state is the core's `CloneState` (a pure machine of its own,
//! since no repo exists yet); the runner is the shell's `CloneRunner`; this
//! view moves events between them and draws the phase the job is in.
//!
//! The helper that answers git's credential questions (`askpass`) is looked
//! for next to the app. Without it a public repository still clones; a
//! private one fails with a message that says what is missing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use gpui::{
    AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement, PathPromptOptions,
    Render, Styled, Subscription, Task, Window, div, px,
};
use invoke_core::clone::{CloneEvent, ClonePhase, CloneRequest, CloneState, step_clone};
use invoke_core::{Failure, OpId, OpOutcome, Progress};
use shell::forge::{ForgeError, ForgeService};
use shell::{CloneRunner, Git};
use tokens::{
    DIALOG, NAME_COLUMN, RADIUS_LG, SPACE_2, SPACE_3, SPACE_4, SPACE_6, SPACE_8, TEXT_BODY,
    TEXT_DISPLAY, TEXT_SMALL,
};
use ui::convert::{hsla, shadows};
use ui::{
    Banner, BannerKind, Button, EditorEvent, ProgressBar, Segmented, TextEditor, TextStyleExt,
    ThemeExt,
};

use crate::auth_prompt::{AuthEvent, AuthPromptView};
use crate::forge_accounts::{AccountsEvent, AccountsView, open_service};
use crate::notice::failure_message;
use crate::repo_picker::{PickerEvent, RepoPicker};

/// What the dialog tells its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloneViewEvent {
    /// The clone worked: open this folder.
    Cloned(PathBuf),
    /// The user closed the dialog.
    Closed,
}

/// The name git would give the folder for `url`: the last part of the
/// address without `.git`. `None` when nothing usable is left.
pub fn name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches(['/', '\\']);
    let last = trimmed.rsplit(['/', '\\', ':']).next()?;
    let name = last.strip_suffix(".git").unwrap_or(last).trim();
    folder_name_ok(name).then(|| name.to_owned())
}

/// A folder name Windows, macOS and Linux all accept for the clone to create
/// (a name ending in a dot is refused on Windows, which also rules out `.`
/// and `..`).
pub fn folder_name_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with('.')
        && !name.ends_with(' ')
        && !name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
}

/// What the dialog shows for the job it is working on.
#[derive(Clone, Debug, PartialEq)]
pub enum Screen {
    /// No job: the form.
    Form,
    Running {
        stage: String,
        /// `None` until git reports a percentage.
        fraction: Option<f32>,
    },
    AwaitingAuth,
    Cancelling,
    Failed(Failure),
    Cancelled,
    Done,
}

/// The screen for the state of the newest job.
pub fn screen(state: &CloneState, job: Option<OpId>) -> Screen {
    let Some(job) = job.and_then(|id| state.job(id)) else {
        return Screen::Form;
    };
    match &job.phase {
        ClonePhase::Running { progress } => match progress {
            Some(Progress { stage, done, total }) => Screen::Running {
                stage: stage.clone(),
                fraction: total
                    .filter(|total| *total > 0)
                    .map(|total| *done as f32 / total as f32),
            },
            None => Screen::Running {
                stage: "Connecting".into(),
                fraction: None,
            },
        },
        ClonePhase::AwaitingAuth(_) => Screen::AwaitingAuth,
        ClonePhase::Cancelling => Screen::Cancelling,
        ClonePhase::Ended(OpOutcome::Succeeded) => Screen::Done,
        ClonePhase::Ended(OpOutcome::Cancelled) => Screen::Cancelled,
        ClonePhase::Ended(OpOutcome::Failed(failure)) => Screen::Failed(failure.clone()),
    }
}

/// Where the helper that answers git's credential questions should be: next
/// to the app.
pub fn helper_path() -> PathBuf {
    std::env::current_exe()
        .map(|exe| {
            exe.with_file_name("askpass")
                .with_extension(std::env::consts::EXE_EXTENSION)
        })
        .unwrap_or_else(|_| PathBuf::from("askpass"))
}

/// Where a clone goes unless the user picks another folder: `Documents` in
/// the home folder, or the home folder itself.
pub fn default_parent() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)?;
    let documents = home.join("Documents");
    Some(if documents.is_dir() { documents } else { home })
}

/// Where the form gets the repository from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// An address typed or pasted.
    Address,
    /// A repository chosen from a connected forge account.
    Account,
    /// The accounts screen, reached from the account list.
    ManageAccounts,
}

/// The optional forge half of the dialog. Nothing in it is contacted until
/// the user opens "From account", and a failure here never blocks cloning
/// by address.
struct ForgeParts {
    service: Result<ForgeService, ForgeError>,
    picker: Option<(Entity<RepoPicker>, Subscription)>,
    accounts: Option<(Entity<AccountsView>, Subscription)>,
}

pub struct CloneView {
    runner: Arc<CloneRunner>,
    helper_missing: bool,
    state: CloneState,
    events: Sender<CloneEvent>,
    /// The job on screen (the newest).
    job: Option<OpId>,
    url: Entity<TextEditor>,
    name: Entity<TextEditor>,
    source: Source,
    forge: ForgeParts,
    parent: Option<PathBuf>,
    auth: Option<(OpId, Entity<AuthPromptView>)>,
    /// The folder the running job clones into.
    target: Option<PathBuf>,
    /// The prompt just appeared: put the cursor in it at the next draw.
    focus_prompt: bool,
    _pump: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloneViewEvent> for CloneView {}

impl CloneView {
    pub fn new(git: Git, cx: &mut Context<Self>) -> Self {
        let helper = helper_path();
        let helper_missing = !helper.exists();
        let (events, inbox) = mpsc::channel();
        let url =
            cx.new(|cx| TextEditor::new("https://github.com/owner/repo.git", 1, cx).single_line());
        let name =
            cx.new(|cx| TextEditor::new("Folder name (from the address)", 1, cx).single_line());
        let subscriptions = vec![
            cx.subscribe(&url, |this, _, event: &EditorEvent, cx| match event {
                EditorEvent::Changed => cx.notify(),
                EditorEvent::Submit => this.start(cx),
            }),
            cx.subscribe(&name, |this, _, event: &EditorEvent, cx| match event {
                EditorEvent::Changed => cx.notify(),
                EditorEvent::Submit => this.start(cx),
            }),
        ];
        Self {
            runner: Arc::new(CloneRunner::new(git, helper)),
            helper_missing,
            state: CloneState::default(),
            events,
            job: None,
            url,
            name,
            source: Source::Address,
            forge: ForgeParts {
                service: open_service(),
                picker: None,
                accounts: None,
            },
            parent: default_parent(),
            auth: None,
            target: None,
            focus_prompt: false,
            _pump: Self::pump(inbox, cx),
            _subscriptions: subscriptions,
        }
    }

    /// Puts the cursor in the address field.
    pub fn focus(&self, window: &mut Window, cx: &mut gpui::App) {
        self.url.update(cx, |url, cx| url.focus(window, cx));
    }

    /// The folder name that will be created: what was typed, or the one the
    /// address gives.
    fn folder_name(&self, cx: &Context<Self>) -> Option<String> {
        let typed = self.name.read(cx).text().trim().to_owned();
        if typed.is_empty() {
            name_from_url(self.url.read(cx).text())
        } else {
            folder_name_ok(&typed).then_some(typed)
        }
    }

    fn target(&self, cx: &Context<Self>) -> Option<PathBuf> {
        Some(self.parent.as_ref()?.join(self.folder_name(cx)?))
    }

    fn can_start(&self, cx: &Context<Self>) -> bool {
        !self.url.read(cx).text().trim().is_empty() && self.target(cx).is_some()
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Clone into".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| {
                    this.parent = Some(path);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Switches where the form gets the repository from. The forge parts are
    /// built the first time they are needed, so nothing is contacted until
    /// the user asks.
    fn show(&mut self, source: Source, cx: &mut Context<Self>) {
        match source {
            Source::Account => self.ensure_picker(cx),
            Source::ManageAccounts => self.ensure_accounts(cx),
            Source::Address => {}
        }
        self.source = source;
        cx.notify();
    }

    fn ensure_picker(&mut self, cx: &mut Context<Self>) {
        if self.forge.picker.is_some() {
            return;
        }
        let Ok(service) = self.forge.service.clone() else {
            return;
        };
        let view = cx.new(|cx| RepoPicker::new(service, cx));
        let subscription = cx.subscribe(&view, |this, _, event: &PickerEvent, cx| match event {
            PickerEvent::Picked(repo) => this.use_address(&repo.clone_url, cx),
            PickerEvent::ManageAccounts => this.show(Source::ManageAccounts, cx),
        });
        self.forge.picker = Some((view, subscription));
    }

    fn ensure_accounts(&mut self, cx: &mut Context<Self>) {
        if self.forge.accounts.is_some() {
            return;
        }
        let Ok(service) = self.forge.service.clone() else {
            return;
        };
        let view = cx.new(|cx| AccountsView::new(service, cx));
        let subscription = cx.subscribe(&view, |this, _, event: &AccountsEvent, cx| match event {
            AccountsEvent::Changed => {
                if let Some((picker, _)) = &this.forge.picker {
                    picker.update(cx, |picker, cx| picker.refresh_accounts(cx));
                }
            }
            AccountsEvent::Closed => {
                // Dropping the screen also stops a sign-in still running.
                this.forge.accounts = None;
                this.show(Source::Account, cx);
            }
        });
        self.forge.accounts = Some((view, subscription));
    }

    /// A repository was chosen from an account: its address goes in the
    /// form, where the folder is chosen and the clone started.
    fn use_address(&mut self, address: &str, cx: &mut Context<Self>) {
        self.url.update(cx, |url, cx| url.set_text(address, cx));
        self.name.update(cx, |name, cx| name.set_text("", cx));
        self.show(Source::Address, cx);
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if !self.can_start(cx) || self.is_busy() {
            return;
        }
        let Some(target) = self.target(cx) else {
            return;
        };
        let url = self.url.read(cx).text().trim().to_owned();
        self.target = Some(target.clone());
        self.job = Some(OpId(self.state.jobs.len() as u64));
        self.deliver(CloneEvent::StartRequested(CloneRequest { url, target }), cx);
    }

    fn is_busy(&self) -> bool {
        matches!(
            screen(&self.state, self.job),
            Screen::Running { .. } | Screen::AwaitingAuth | Screen::Cancelling
        )
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = self.job {
            self.deliver(CloneEvent::CancelRequested(job), cx);
        }
    }

    /// Back to the form, keeping what was typed.
    fn again(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = self.job.take() {
            self.deliver(CloneEvent::Dismissed(job), cx);
        }
        self.auth = None;
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.again(cx);
        cx.emit(CloneViewEvent::Closed);
    }

    /// Steps the machine with one event and runs what it asks for.
    fn deliver(&mut self, event: CloneEvent, cx: &mut Context<Self>) {
        let (state, commands) = step_clone(std::mem::take(&mut self.state), event);
        self.state = state;
        for command in commands {
            self.runner.execute(command, self.events.clone());
        }
        self.after_step(cx);
        cx.notify();
    }

    /// Reacts to the phase the job is now in: a prompt gets its form, a
    /// finished clone is handed to the owner.
    fn after_step(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.job else { return };
        match screen(&self.state, Some(job)) {
            Screen::AwaitingAuth => {
                let prompt = match self.state.job(job).map(|j| &j.phase) {
                    Some(ClonePhase::AwaitingAuth(prompt)) => prompt.clone(),
                    _ => return,
                };
                let view = cx.new(|cx| AuthPromptView::new(prompt, cx));
                cx.subscribe(&view, |this, _, event: &AuthEvent, cx| match event {
                    AuthEvent::Answer(answer) => {
                        if let Some(job) = this.job {
                            this.auth = None;
                            this.deliver(CloneEvent::AuthAnswered(job, answer.clone()), cx);
                        }
                    }
                    AuthEvent::Cancel => {
                        this.auth = None;
                        this.cancel(cx);
                    }
                })
                .detach();
                self.auth = Some((job, view));
                self.focus_prompt = true;
            }
            Screen::Done => {
                self.auth = None;
                if let Some(target) = self.target.take() {
                    cx.emit(CloneViewEvent::Cloned(target));
                }
            }
            _ => {
                if !matches!(screen(&self.state, Some(job)), Screen::AwaitingAuth) {
                    self.auth = None;
                }
            }
        }
    }

    /// Waits for the runner's events on a background thread and steps the
    /// machine with each batch on the UI thread.
    fn pump(inbox: Receiver<CloneEvent>, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let mut inbox = inbox;
            loop {
                let (back, batch) = cx
                    .background_executor()
                    .spawn(async move {
                        let mut batch = Vec::new();
                        if let Ok(first) = inbox.recv() {
                            batch.push(first);
                            while let Ok(next) = inbox.try_recv() {
                                batch.push(next);
                            }
                        }
                        (inbox, batch)
                    })
                    .await;
                inbox = back;
                if batch.is_empty() {
                    break;
                }
                let alive = this.update(cx, |this, cx| {
                    for event in batch {
                        this.deliver(event, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
    }
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

impl Render for CloneView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let screen = screen(&self.state, self.job);
        if self.source == Source::ManageAccounts
            && matches!(screen, Screen::Form | Screen::Failed(_) | Screen::Cancelled)
            && let Some((view, _)) = &self.forge.accounts
        {
            return div().flex().flex_1().child(view.clone());
        }
        let (start, browse, close, cancel, again) = (
            cx.entity(),
            cx.entity(),
            cx.entity(),
            cx.entity(),
            cx.entity(),
        );
        let mut body = div()
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
                    .child("Clone a repository"),
            );
        if self.helper_missing {
            body = body.child(Banner::new(
                BannerKind::Warning,
                format!(
                    "The credential helper was not found at {}. Public repositories clone; a private one cannot ask for your password until it is built (cargo build -p askpass).",
                    path_text(&helper_path())
                ),
            ));
        }
        match screen {
            Screen::Form | Screen::Failed(_) | Screen::Cancelled => {
                match &screen {
                    Screen::Failed(failure) => {
                        let message = failure_message(failure);
                        let mut banner = Banner::new(
                            if message.tone == crate::notice::Tone::Error {
                                BannerKind::Error
                            } else {
                                BannerKind::Warning
                            },
                            message.text,
                        );
                        if let Some(details) = message.details {
                            banner = banner.action(
                                div()
                                    .max_w(px(NAME_COLUMN))
                                    .truncate()
                                    .text_style(&TEXT_SMALL, cx)
                                    .child(crate::notice::headline(&details)),
                            );
                        }
                        body = body.child(banner);
                    }
                    Screen::Cancelled => {
                        body = body.child(Banner::new(
                            BannerKind::Warning,
                            "Cancelled. Nothing was left in the folder.",
                        ));
                    }
                    _ => {}
                }
                let sources = cx.entity();
                body = body.child(Segmented::new(
                    "clone-source",
                    vec!["Address".into(), "From account".into()],
                    usize::from(self.source != Source::Address),
                    move |index, _, cx| {
                        sources.update(cx, |this, cx| {
                            this.show(
                                if index == 0 {
                                    Source::Address
                                } else {
                                    Source::Account
                                },
                                cx,
                            )
                        })
                    },
                ));
                if self.source == Source::Account {
                    body = match (&self.forge.picker, &self.forge.service) {
                        (Some((picker, _)), _) => body.child(picker.clone()),
                        (None, Err(error)) => body.child(Banner::new(
                            BannerKind::Warning,
                            format!(
                                "Accounts are not available: {} You can still clone by address.",
                                error.user_message()
                            ),
                        )),
                        (None, Ok(_)) => body,
                    };
                    body = body.child(
                        div().flex().child(
                            Button::new("clone-account-close", "Cancel")
                                .ghost()
                                .on_click(move |_, _, cx| {
                                    close.update(cx, |this, cx| this.close(cx))
                                }),
                        ),
                    );
                } else {
                    let target = self.target(cx);
                    let can_start = self.can_start(cx);
                    let label = |text: &'static str, cx: &mut Context<Self>| {
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(text)
                    };
                    body = body
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(SPACE_2))
                                .child(label("Address", cx))
                                .child(self.url.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(SPACE_2))
                                .child(label("Clone into", cx))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(SPACE_3))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w(px(0.))
                                                .truncate()
                                                .text_style(&TEXT_BODY, cx)
                                                .text_color(hsla(if self.parent.is_some() {
                                                    p.ink
                                                } else {
                                                    p.ink_faint
                                                }))
                                                .child(match &self.parent {
                                                    Some(parent) => path_text(parent),
                                                    None => "No folder chosen".into(),
                                                }),
                                        )
                                        .child(
                                            Button::new("clone-browse", "Choose folder…").on_click(
                                                move |_, _, cx| {
                                                    browse.update(cx, |this, cx| this.browse(cx))
                                                },
                                            ),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(SPACE_2))
                                .child(label("Folder name", cx))
                                .child(self.name.clone()),
                        )
                        .child(
                            div()
                                .text_style(&TEXT_SMALL, cx)
                                .text_color(hsla(p.ink_muted))
                                .child(match target {
                                    Some(target) => format!("Will create {}", path_text(&target)),
                                    None => "Enter an address and choose a folder.".into(),
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(SPACE_2))
                                .child(
                                    Button::new("clone-start", "Clone")
                                        .primary()
                                        .disabled(!can_start)
                                        .on_click(move |_, _, cx| {
                                            start.update(cx, |this, cx| this.start(cx))
                                        }),
                                )
                                .child(Button::new("clone-close", "Cancel").ghost().on_click(
                                    move |_, _, cx| close.update(cx, |this, cx| this.close(cx)),
                                )),
                        );
                    let _ = again;
                }
            }
            Screen::Running { stage, fraction } => {
                body = body
                    .child(
                        div()
                            .text_style(&TEXT_BODY, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(match &self.target {
                                Some(target) => format!("Cloning into {}", path_text(target)),
                                None => "Cloning".into(),
                            }),
                    )
                    .children(fraction.map(ProgressBar::new))
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(match fraction {
                                Some(fraction) => format!("{stage}: {:.0}%", fraction * 100.0),
                                None => stage,
                            }),
                    )
                    .child(div().flex().child(
                        Button::new("clone-cancel", "Cancel").on_click(move |_, _, cx| {
                            cancel.update(cx, |this, cx| this.cancel(cx))
                        }),
                    ));
            }
            Screen::AwaitingAuth => {
                if let Some((_, view)) = &self.auth {
                    if std::mem::take(&mut self.focus_prompt) {
                        view.update(cx, |view, cx| view.focus(window, cx));
                    }
                    body = body.child(view.clone());
                }
            }
            Screen::Cancelling => {
                body = body.child(
                    div()
                        .text_style(&TEXT_BODY, cx)
                        .text_color(hsla(p.ink_muted))
                        .child("Cancelling and cleaning up…"),
                );
            }
            Screen::Done => {
                body = body.child(
                    div()
                        .text_style(&TEXT_BODY, cx)
                        .text_color(hsla(p.ink_muted))
                        .child("Done. Opening the repository…"),
                );
            }
        }
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .p(px(SPACE_8))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use invoke_core::clone::CloneJob;
    use invoke_core::{AuthKind, AuthPrompt, ErrorClass};

    use super::*;

    #[test]
    fn the_folder_name_comes_from_the_address() {
        for (url, name) in [
            ("https://github.com/owner/repo.git", Some("repo")),
            ("https://github.com/owner/repo", Some("repo")),
            ("https://github.com/owner/repo/", Some("repo")),
            ("git@github.com:owner/repo.git", Some("repo")),
            ("ssh://git@host:22/team/tool.git", Some("tool")),
            (r"C:\work\old-project", Some("old-project")),
            ("  https://h.example/x/y.git  ", Some("y")),
            ("", None),
            ("git@host:", None),
            ("https://host/..", None),
        ] {
            assert_eq!(name_from_url(url).as_deref(), name, "{url}");
        }
    }

    #[test]
    fn folder_names_all_three_systems_accept() {
        assert!(folder_name_ok("repo"));
        assert!(folder_name_ok("my repo.v2"));
        for bad in [
            "", ".", "..", "a/b", r"a\b", "a:b", "a*", "a?", "a|b", "a\"", "x.", "x ", "a\nb",
        ] {
            assert!(!folder_name_ok(bad), "{bad:?}");
        }
    }

    fn state_with(phase: ClonePhase) -> (CloneState, OpId) {
        let (mut state, _) = step_clone(
            CloneState::default(),
            CloneEvent::StartRequested(CloneRequest {
                url: "u".into(),
                target: PathBuf::from("/x/y"),
            }),
        );
        let id = state.jobs[0].id;
        let job: &mut CloneJob = &mut state.jobs[0];
        job.phase = phase;
        (state, id)
    }

    #[test]
    fn the_screen_follows_the_phase_of_the_job() {
        let (state, id) = state_with(ClonePhase::Running { progress: None });
        assert_eq!(
            screen(&state, Some(id)),
            Screen::Running {
                stage: "Connecting".into(),
                fraction: None
            }
        );
        let (state, id) = state_with(ClonePhase::Running {
            progress: Some(Progress {
                stage: "Receiving objects".into(),
                done: 25,
                total: Some(100),
            }),
        });
        assert_eq!(
            screen(&state, Some(id)),
            Screen::Running {
                stage: "Receiving objects".into(),
                fraction: Some(0.25)
            }
        );
        let (state, id) = state_with(ClonePhase::AwaitingAuth(AuthPrompt {
            kind: AuthKind::Password,
            text: "Password:".into(),
        }));
        assert_eq!(screen(&state, Some(id)), Screen::AwaitingAuth);
        let (state, id) = state_with(ClonePhase::Cancelling);
        assert_eq!(screen(&state, Some(id)), Screen::Cancelling);
        let (state, id) = state_with(ClonePhase::Ended(OpOutcome::Succeeded));
        assert_eq!(screen(&state, Some(id)), Screen::Done);
        let (state, id) = state_with(ClonePhase::Ended(OpOutcome::Cancelled));
        assert_eq!(screen(&state, Some(id)), Screen::Cancelled);
        let failure = Failure {
            class: ErrorClass::Auth,
            details: "denied".into(),
        };
        let (state, id) = state_with(ClonePhase::Ended(OpOutcome::Failed(failure.clone())));
        assert_eq!(screen(&state, Some(id)), Screen::Failed(failure));
        // No job, or one that was dismissed: the form.
        assert_eq!(screen(&state, None), Screen::Form);
        assert_eq!(screen(&CloneState::default(), Some(OpId(7))), Screen::Form);
    }

    #[test]
    fn a_total_of_zero_shows_no_percentage() {
        let (state, id) = state_with(ClonePhase::Running {
            progress: Some(Progress {
                stage: "Counting".into(),
                done: 3,
                total: Some(0),
            }),
        });
        assert!(matches!(
            screen(&state, Some(id)),
            Screen::Running { fraction: None, .. }
        ));
    }
}
