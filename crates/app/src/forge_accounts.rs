//! Forge accounts: connect GitHub or GitLab, see who is connected, choose
//! the account a host uses, and disconnect. Optional: nothing in the app
//! waits for it, and every git feature works with no account.
//!
//! The sign-in itself is the core's `SignInPhase` machine; the shell's
//! `SignInRunner` does its IO. This view moves events between them and draws
//! the phase. Tokens never pass through here except the one the user types
//! into the masked field, which goes straight to the service and is cleared.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use gpui::{
    AppContext, ClipboardItem, Context, Entity, EventEmitter, IntoElement, ParentElement, Render,
    Styled, Subscription, Task, Window, div, px,
};
use invoke_core::forge::{
    AccountBook, Attempt, AuthMethod, ForgeAccount, ForgeKind, SignInCommand, SignInEvent,
    SignInFailure, SignInMethod, SignInPhase, UserPrompt, step_sign_in,
};
use shell::forge::{ForgeError, ForgeService, KeyringStore, ServiceConfig, SignInRunner};
use tokens::{
    DIALOG, SPACE_1, SPACE_2, SPACE_3, SPACE_4, TEXT_BODY, TEXT_BODY_MEDIUM, TEXT_SMALL, TEXT_TITLE,
};
use ui::convert::hsla;
use ui::{
    Banner, BannerKind, Button, Divider, EditorEvent, Pill, Segmented, TextEditor, TextStyleExt,
    ThemeExt,
};

use crate::persist;

/// What the view tells its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountsEvent {
    /// The list of accounts changed (connected, disconnected, preferred).
    Changed,
    /// The user is done.
    Closed,
}

/// The forge service: the OS keychain for tokens, and the account list in
/// the app data folder. One per process, shared by every window, so their
/// writes are serialized by one lock (the service also checks the file
/// against what it last saw before writing, for other processes). An error
/// here only switches the forge features off, and is retried next time.
pub fn open_service() -> Result<ForgeService, ForgeError> {
    static SHARED: std::sync::Mutex<Option<ForgeService>> = std::sync::Mutex::new(None);
    shared_or_open(&SHARED, || {
        let path = persist::data_dir().map(|dir| dir.join("forge-accounts.txt"));
        ForgeService::new(
            Arc::new(KeyringStore::new()),
            ServiceConfig::default(),
            path,
        )
    })
}

/// The value in `slot`, or a new one from `open` kept there. A failure is
/// not kept, so the next call tries again.
pub fn shared_or_open<T: Clone, E>(
    slot: &std::sync::Mutex<Option<T>>,
    open: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(value) = slot.as_ref() {
        return Ok(value.clone());
    }
    let value = open()?;
    *slot = Some(value.clone());
    Ok(value)
}

/// A change to the connected accounts the user asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountChange {
    Disconnect(String),
    Prefer(String),
}

/// What a change came to: whether the list changed, and what to tell the
/// user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeOutcome {
    pub changed: bool,
    pub notice: Option<(BannerKind, String)>,
}

/// Carries out a change. It blocks on the keychain and the disk, so it is
/// only ever called on a background thread.
pub fn apply_change(service: &ForgeService, change: &AccountChange) -> ChangeOutcome {
    let result = match change {
        AccountChange::Disconnect(key) => service.disconnect(key),
        AccountChange::Prefer(key) => service.prefer(key),
    };
    match (result, change) {
        (Ok(()), AccountChange::Disconnect(_)) => ChangeOutcome {
            changed: true,
            notice: Some((
                BannerKind::Warning,
                "Disconnected. The token was removed from the keychain.".into(),
            )),
        },
        (Ok(()), AccountChange::Prefer(_)) => ChangeOutcome {
            changed: true,
            notice: None,
        },
        (Err(error), _) => ChangeOutcome {
            changed: false,
            notice: Some((BannerKind::Error, error.user_message())),
        },
    }
}

/// What the sign-in area shows.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInScreen {
    /// Nothing running.
    Idle,
    Starting(SignInMethod),
    /// GitHub: type this code on that page.
    Device {
        user_code: String,
        verification_url: String,
    },
    /// GitLab: approve in the browser.
    Browser {
        url: String,
    },
    Connected(String),
    Failed(String),
}

pub fn sign_in_screen(phase: &SignInPhase) -> SignInScreen {
    match phase {
        SignInPhase::Idle => SignInScreen::Idle,
        SignInPhase::Requesting { method, .. } => SignInScreen::Starting(*method),
        SignInPhase::AwaitingUser { prompt, .. } => match prompt {
            UserPrompt::DeviceCode {
                user_code,
                verification_url,
            } => SignInScreen::Device {
                user_code: user_code.clone(),
                verification_url: verification_url.clone(),
            },
            UserPrompt::Browser { url } => SignInScreen::Browser { url: url.clone() },
        },
        SignInPhase::Authorized(account) => SignInScreen::Connected(account.label()),
        SignInPhase::Failed(failure) => SignInScreen::Failed(failure_text(failure)),
    }
}

pub fn failure_text(failure: &SignInFailure) -> String {
    match failure {
        SignInFailure::Denied => "The request was declined on the website.".into(),
        SignInFailure::Expired => "The code expired. Start again.".into(),
        SignInFailure::Cancelled(_) => "Cancelled.".into(),
        SignInFailure::Error(message) => message.clone(),
        SignInFailure::CancelledWithLeftover(message) => {
            format!("Cancelled, but it could not be fully undone. {message}")
        }
    }
}

/// An address safe to hand to the system's URL opener: https, or http to
/// this machine (tests and local servers). Nothing else (`file:`, custom
/// protocols, look-alike hosts) is ever opened.
pub fn is_web_url(url: &str) -> bool {
    shell::forge::http::origin_of_url(url).is_some_and(|origin| origin.is_secure())
}

/// A line under an account: how it signed in.
pub fn method_text(method: AuthMethod) -> &'static str {
    match method {
        AuthMethod::OAuth => "Signed in on the website",
        AuthMethod::Pat => "Access token",
    }
}

/// Where a pasted token is meant to go: the forge and, for GitLab, the
/// server. Two targets are the same only when the token is valid for both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenTarget {
    kind: ForgeKind,
    /// The server as typed, trimmed and lower case; empty for GitHub (it has
    /// one server) and for gitlab.com typed as nothing.
    instance: String,
}

impl TokenTarget {
    pub fn new(kind: ForgeKind, instance_text: &str) -> Self {
        let instance = match kind {
            ForgeKind::GitHub => String::new(),
            ForgeKind::GitLab => instance_text.trim().to_lowercase(),
        };
        Self { kind, instance }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panel {
    List,
    Token,
}

pub struct AccountsView {
    service: ForgeService,
    runner: Arc<SignInRunner>,
    phase: SignInPhase,
    events: Sender<SignInEvent>,
    panel: Panel,
    token_kind: ForgeKind,
    /// Where the token field's content was meant to go.
    target: TokenTarget,
    token: Entity<TextEditor>,
    instance: Entity<TextEditor>,
    /// A token is being checked with the server.
    checking: bool,
    /// A disconnect or preference change running in the background.
    changing: Option<AccountChange>,
    /// The number the next sign-in attempt gets. Answers carry it back, so a
    /// late answer for an earlier attempt is dropped by the machine.
    next_attempt: u64,
    notice: Option<(BannerKind, String)>,
    _pump: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<AccountsEvent> for AccountsView {}

impl Drop for AccountsView {
    /// Closing the screen mid sign-in stops the poll timer or the local
    /// listener, so nothing keeps running unseen. What to send comes from the
    /// sign-in machine (see [`commands_on_close`]): a flow that already
    /// finished, as "Connected" shows, is never undone by closing.
    fn drop(&mut self) {
        for command in commands_on_close(&self.phase) {
            self.runner.execute(command, self.events.clone());
        }
    }
}

/// What closing the screen asks of the runner in this phase. It is the
/// machine's own answer to Cancel, so only a flow still running is stopped:
/// after success (`Authorized`) or any other end there is nothing to send,
/// and closing the view cannot roll back a connected account.
pub fn commands_on_close(phase: &SignInPhase) -> Vec<SignInCommand> {
    step_sign_in(phase.clone(), SignInEvent::Cancel).1
}

impl AccountsView {
    pub fn new(service: ForgeService, cx: &mut Context<Self>) -> Self {
        // Pick up changes another process made to the list: off the UI
        // thread, since it reads the disk.
        let reloading = service.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .spawn(async move { reloading.reload() })
                .await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
        let (events, inbox) = mpsc::channel();
        let token = cx.new(|cx| TextEditor::new("Paste the access token", 1, cx).masked());
        let instance = cx.new(|cx| {
            TextEditor::new("https://gitlab.example.com (empty: gitlab.com)", 1, cx).single_line()
        });
        let subscriptions = vec![
            cx.subscribe(&token, |this, _, event: &EditorEvent, cx| match event {
                EditorEvent::Changed => cx.notify(),
                EditorEvent::Submit => this.connect_token(cx),
            }),
            cx.subscribe(&instance, |this, _, event: &EditorEvent, cx| match event {
                EditorEvent::Changed => this.retarget(cx),
                EditorEvent::Submit => this.connect_token(cx),
            }),
        ];
        Self {
            runner: Arc::new(SignInRunner::new(service.clone())),
            service,
            phase: SignInPhase::Idle,
            events,
            panel: Panel::List,
            token_kind: ForgeKind::GitHub,
            target: TokenTarget::new(ForgeKind::GitHub, ""),
            token,
            instance,
            checking: false,
            changing: None,
            next_attempt: 1,
            notice: None,
            _pump: Self::pump(inbox, cx),
            _subscriptions: subscriptions,
        }
    }

    // -- sign in on the website -------------------------------------------------

    fn begin(&mut self, method: SignInMethod, cx: &mut Context<Self>) {
        self.notice = None;
        let attempt = Attempt(self.next_attempt);
        self.next_attempt += 1;
        self.deliver(SignInEvent::Begin { method, attempt }, cx);
    }

    fn cancel_sign_in(&mut self, cx: &mut Context<Self>) {
        self.deliver(SignInEvent::Cancel, cx);
    }

    fn deliver(&mut self, event: SignInEvent, cx: &mut Context<Self>) {
        let before = self.phase.clone();
        let (phase, commands) =
            step_sign_in(std::mem::replace(&mut self.phase, SignInPhase::Idle), event);
        self.phase = phase;
        for command in commands {
            let command: SignInCommand = command;
            self.runner.execute(command, self.events.clone());
        }
        match (&before, &self.phase) {
            (SignInPhase::Requesting { .. }, SignInPhase::AwaitingUser { prompt, .. }) => {
                // Take the user to the page; the code (or the approval
                // button) is waiting there.
                let url = match prompt {
                    UserPrompt::DeviceCode {
                        verification_url, ..
                    } => verification_url,
                    UserPrompt::Browser { url } => url,
                };
                if is_web_url(url) {
                    cx.open_url(url);
                }
            }
            (_, SignInPhase::Authorized(_)) if !matches!(before, SignInPhase::Authorized(_)) => {
                cx.emit(AccountsEvent::Changed);
            }
            _ => {}
        }
        cx.notify();
    }

    fn pump(inbox: Receiver<SignInEvent>, cx: &mut Context<Self>) -> Task<()> {
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

    // -- sign in with a token -----------------------------------------------------

    /// The forge or the server changed: whatever is in the token field was
    /// meant for the old one, so it is cleared rather than sent to the new
    /// one.
    fn retarget(&mut self, cx: &mut Context<Self>) {
        let target = TokenTarget::new(self.token_kind, self.instance.read(cx).text());
        if target != self.target {
            self.target = target;
            self.token.update(cx, |token, cx| token.set_text("", cx));
            self.notice = None;
        }
        cx.notify();
    }

    fn can_connect_token(&self, cx: &Context<Self>) -> bool {
        !self.checking && !self.token.read(cx).is_empty()
    }

    fn connect_token(&mut self, cx: &mut Context<Self>) {
        if !self.can_connect_token(cx) {
            return;
        }
        let kind = self.token_kind;
        let token = self.token.read(cx).text().to_owned();
        let instance = self.instance.read(cx).text().to_owned();
        let service = self.service.clone();
        self.checking = true;
        self.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { service.connect_with_token(kind, &instance, &token) })
                .await;
            this.update(cx, |this, cx| {
                this.checking = false;
                match result {
                    Ok(account) => {
                        // The token has done its job: do not keep it on screen.
                        this.token.update(cx, |token, cx| token.set_text("", cx));
                        this.panel = Panel::List;
                        this.notice = Some((
                            BannerKind::Warning,
                            format!("Connected {}.", account.label()),
                        ));
                        cx.emit(AccountsEvent::Changed);
                    }
                    Err(error) => {
                        this.notice = Some((BannerKind::Error, error.user_message()));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // -- the list -----------------------------------------------------------------

    /// Disconnect and preference changes touch the keychain and the disk, so
    /// they run in the background: a slow or stuck keychain never freezes the
    /// window. One change at a time.
    fn change(&mut self, change: AccountChange, cx: &mut Context<Self>) {
        if self.changing.is_some() {
            return;
        }
        self.changing = Some(change.clone());
        self.notice = None;
        let service = self.service.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { apply_change(&service, &change) })
                .await;
            this.update(cx, |this, cx| {
                this.changing = None;
                if outcome.changed {
                    cx.emit(AccountsEvent::Changed);
                }
                this.notice = outcome.notice;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn show_token_panel(&mut self, cx: &mut Context<Self>) {
        self.panel = Panel::Token;
        self.notice = None;
        cx.notify();
    }

    fn show_list(&mut self, cx: &mut Context<Self>) {
        self.panel = Panel::List;
        cx.notify();
    }

    fn sign_in_busy(&self) -> bool {
        self.phase.is_active()
    }
}

/// Whether another account is connected to the same server. A preference is
/// kept per origin (scheme, host and port), so the same host on another port
/// is another server and does not compete.
pub fn shares_origin(book: &AccountBook, account: &ForgeAccount) -> bool {
    book.accounts()
        .iter()
        .filter(|other| other.origin == account.origin)
        .count()
        > 1
}

fn host_of(account: &ForgeAccount) -> String {
    account
        .origin
        .as_str()
        .trim_start_matches("https://")
        .to_owned()
}

impl Render for AccountsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let muted = |cx: &mut Context<Self>| {
            div()
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink_muted))
        };
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(SPACE_4))
            .w(px(DIALOG))
            .child(
                div()
                    .text_style(&TEXT_TITLE, cx)
                    .text_color(hsla(p.ink))
                    .child("Accounts"),
            );
        body = body.child(muted(cx).child(
            "Optional. Connect GitHub or GitLab to pick repositories to clone. Git itself keeps using your usual credentials, and tokens stay in the system keychain.",
        ));
        if let Some((kind, text)) = &self.notice {
            body = body.child(Banner::new(*kind, text.clone()));
        }

        if self.panel == Panel::Token {
            body = body.child(self.render_token_panel(cx));
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .child(body);
        }

        // Sign-in area.
        match sign_in_screen(&self.phase) {
            SignInScreen::Idle | SignInScreen::Connected(_) => {}
            SignInScreen::Starting(_) => {
                body = body.child(muted(cx).child("Contacting the server…"));
            }
            SignInScreen::Device {
                user_code,
                verification_url,
            } => {
                let (copy, open, cancel) =
                    (user_code.clone(), verification_url.clone(), cx.entity());
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(SPACE_2))
                        .p(px(SPACE_3))
                        .rounded(px(tokens::RADIUS_MD))
                        .bg(hsla(p.bg_raised))
                        .child(
                            muted(cx)
                                .child(format!("Open {verification_url} and enter this code:")),
                        )
                        .child(
                            div()
                                .text_style(&TEXT_TITLE, cx)
                                .text_color(hsla(p.ink))
                                .child(user_code),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(SPACE_2))
                                .child(Button::new("forge-copy-code", "Copy code").on_click(
                                    move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy.clone(),
                                        ))
                                    },
                                ))
                                .child(Button::new("forge-open-page", "Open page").on_click(
                                    move |_, _, cx| {
                                        if is_web_url(&open) {
                                            cx.open_url(&open);
                                        }
                                    },
                                ))
                                .child(
                                    Button::new("forge-cancel-sign-in", "Cancel")
                                        .ghost()
                                        .on_click(move |_, _, cx| {
                                            cancel.update(cx, |this, cx| this.cancel_sign_in(cx))
                                        }),
                                ),
                        )
                        .child(muted(cx).child("Waiting for you to approve it…")),
                );
            }
            SignInScreen::Browser { url } => {
                let (open, cancel) = (url, cx.entity());
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(SPACE_2))
                        .p(px(SPACE_3))
                        .rounded(px(tokens::RADIUS_MD))
                        .bg(hsla(p.bg_raised))
                        .child(
                            muted(cx).child("Approve Invoke Git in the browser tab that opened."),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(SPACE_2))
                                .child(Button::new("forge-reopen", "Open again").on_click(
                                    move |_, _, cx| {
                                        if is_web_url(&open) {
                                            cx.open_url(&open);
                                        }
                                    },
                                ))
                                .child(
                                    Button::new("forge-cancel-browser", "Cancel")
                                        .ghost()
                                        .on_click(move |_, _, cx| {
                                            cancel.update(cx, |this, cx| this.cancel_sign_in(cx))
                                        }),
                                ),
                        ),
                );
            }
            SignInScreen::Failed(message) => {
                body = body.child(Banner::new(BannerKind::Error, message));
            }
        }

        // Accounts.
        let book = self.service.book();
        if book.is_empty() {
            body = body.child(muted(cx).child("No account connected."));
        } else {
            body = body.child(self.render_accounts(&book, cx));
        }

        let busy = self.sign_in_busy();
        let (github, gitlab, token, close) = (cx.entity(), cx.entity(), cx.entity(), cx.entity());
        body = body.child(Divider::horizontal()).child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(SPACE_2))
                .child(
                    Button::new("forge-connect-github", "Connect GitHub")
                        .primary()
                        .disabled(busy)
                        .on_click(move |_, _, cx| {
                            github.update(cx, |this, cx| this.begin(SignInMethod::GitHubDevice, cx))
                        }),
                )
                .child(
                    Button::new("forge-connect-gitlab", "Connect GitLab.com")
                        .disabled(busy)
                        .on_click(move |_, _, cx| {
                            gitlab
                                .update(cx, |this, cx| this.begin(SignInMethod::GitLabBrowser, cx))
                        }),
                )
                .child(
                    Button::new("forge-add-token", "Use an access token…")
                        .disabled(busy)
                        .on_click(move |_, _, cx| {
                            token.update(cx, |this, cx| this.show_token_panel(cx))
                        }),
                )
                .child(div().flex_1())
                .child(
                    Button::new("forge-done", "Done")
                        .ghost()
                        .on_click(move |_, _, cx| {
                            close.update(cx, |_, cx| cx.emit(AccountsEvent::Closed))
                        }),
                ),
        );
        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .child(body)
    }
}

impl AccountsView {
    fn render_accounts(&self, book: &AccountBook, cx: &mut Context<Self>) -> gpui::AnyElement {
        let p = cx.palette();
        let mut list = div().flex().flex_col().gap(px(SPACE_2));
        for (index, account) in book.accounts().iter().enumerate() {
            let key = account.key();
            let (disconnect, prefer) = (cx.entity(), cx.entity());
            let (disconnect_key, prefer_key) = (key.clone(), key.clone());
            let shares_host = shares_origin(book, account);
            let preferred = shares_host && book.is_preferred(&key);
            let mut actions = div().flex().items_center().gap(px(SPACE_2));
            if preferred {
                actions = actions.child(Pill::new(format!("Used for {}", host_of(account))));
            } else if shares_host {
                actions = actions.child(
                    Button::new(
                        ("forge-prefer", index),
                        format!("Use for {}", host_of(account)),
                    )
                    .ghost()
                    .on_click(move |_, _, cx| {
                        prefer.update(cx, |this, cx| {
                            this.change(AccountChange::Prefer(prefer_key.clone()), cx)
                        })
                    }),
                );
            }
            actions = actions.child(
                Button::new(("forge-disconnect", index), "Disconnect")
                    .ghost()
                    .on_click(move |_, _, cx| {
                        disconnect.update(cx, |this, cx| {
                            this.change(AccountChange::Disconnect(disconnect_key.clone()), cx)
                        })
                    }),
            );
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(SPACE_3))
                    .p(px(SPACE_2))
                    .rounded(px(tokens::RADIUS_MD))
                    .bg(hsla(p.bg_raised))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w(px(0.))
                            .gap(px(SPACE_1))
                            .child(
                                div()
                                    .truncate()
                                    .text_style(&TEXT_BODY_MEDIUM, cx)
                                    .text_color(hsla(p.ink))
                                    .child(format!(
                                        "{} · {}",
                                        account.kind.label(),
                                        account.username
                                    )),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_style(&TEXT_SMALL, cx)
                                    .text_color(hsla(p.ink_muted))
                                    .child(format!(
                                        "{} · {}",
                                        host_of(account),
                                        method_text(account.method)
                                    )),
                            ),
                    )
                    .child(actions),
            );
        }
        list.into_any_element()
    }

    fn render_token_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let p = cx.palette();
        let (pick, connect, back) = (cx.entity(), cx.entity(), cx.entity());
        let can = self.can_connect_token(cx);
        let kind = self.token_kind;
        let label = |text: &'static str, cx: &mut Context<Self>| {
            div()
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink_muted))
                .child(text)
        };
        let mut panel = div().flex().flex_col().gap(px(SPACE_3)).child(label(
            "A personal access token. Create one on the website with read access to your repositories (GitHub: scope repo; GitLab: read_api). It is checked once, then kept only in the system keychain.",
            cx,
        ));
        panel = panel.child(Segmented::new(
            "forge-token-kind",
            vec!["GitHub".into(), "GitLab (gitlab.com or self-hosted)".into()],
            if kind == ForgeKind::GitHub { 0 } else { 1 },
            move |index, _, cx| {
                pick.update(cx, |this, cx| {
                    this.token_kind = if index == 0 {
                        ForgeKind::GitHub
                    } else {
                        ForgeKind::GitLab
                    };
                    this.retarget(cx);
                })
            },
        ));
        if kind == ForgeKind::GitLab {
            panel = panel.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_2))
                    .child(label("Server address", cx))
                    .child(self.instance.clone()),
            );
        }
        panel
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_2))
                    .child(label("Token", cx))
                    .child(self.token.clone()),
            )
            .child(
                div()
                    .flex()
                    .gap(px(SPACE_2))
                    .child(
                        Button::new(
                            "forge-token-connect",
                            if self.checking {
                                "Checking…"
                            } else {
                                "Connect"
                            },
                        )
                        .primary()
                        .disabled(!can)
                        .on_click(move |_, _, cx| {
                            connect.update(cx, |this, cx| this.connect_token(cx))
                        }),
                    )
                    .child(
                        Button::new("forge-token-back", "Back").ghost().on_click(
                            move |_, _, cx| back.update(cx, |this, cx| this.show_list(cx)),
                        ),
                    ),
            )
            .text_style(&TEXT_BODY, cx)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use invoke_core::forge::Origin;

    use super::*;

    fn account() -> ForgeAccount {
        ForgeAccount::new(
            ForgeKind::GitHub,
            Origin::parse("github.com").unwrap(),
            "ana",
            AuthMethod::OAuth,
        )
        .unwrap()
    }

    #[test]
    fn the_screen_follows_the_phase() {
        assert_eq!(sign_in_screen(&SignInPhase::Idle), SignInScreen::Idle);
        assert_eq!(
            sign_in_screen(&SignInPhase::Requesting {
                method: SignInMethod::GitHubDevice,
                attempt: Attempt(1)
            }),
            SignInScreen::Starting(SignInMethod::GitHubDevice)
        );
        let (phase, _) = step_sign_in(
            SignInPhase::Requesting {
                method: SignInMethod::GitHubDevice,
                attempt: Attempt(1),
            },
            SignInEvent::Reply {
                attempt: Attempt(1),
                reply: invoke_core::forge::SignInReply::PromptReady {
                    prompt: UserPrompt::DeviceCode {
                        user_code: "AB-12".into(),
                        verification_url: "https://github.com/login/device".into(),
                    },
                    interval_secs: 5,
                },
            },
        );
        assert_eq!(
            sign_in_screen(&phase),
            SignInScreen::Device {
                user_code: "AB-12".into(),
                verification_url: "https://github.com/login/device".into()
            }
        );
        assert_eq!(
            sign_in_screen(&SignInPhase::Authorized(account())),
            SignInScreen::Connected("ana @ github.com".into())
        );
        assert_eq!(
            sign_in_screen(&SignInPhase::Failed(SignInFailure::Denied)),
            SignInScreen::Failed("The request was declined on the website.".into())
        );
    }

    #[test]
    fn a_token_is_kept_only_while_its_target_is_unchanged() {
        let gitlab = TokenTarget::new(ForgeKind::GitLab, "https://git.corp");
        // Switching forge, or editing the server, is a new target (the
        // token field is cleared).
        assert_ne!(TokenTarget::new(ForgeKind::GitHub, ""), gitlab);
        assert_ne!(
            gitlab,
            TokenTarget::new(ForgeKind::GitLab, "https://git.corp.evil.example")
        );
        assert_ne!(
            gitlab,
            TokenTarget::new(ForgeKind::GitLab, "https://git.cor")
        );
        assert_ne!(gitlab, TokenTarget::new(ForgeKind::GitLab, ""));
        // The forge alone is enough to differ (GitHub vs gitlab.com).
        assert_ne!(
            TokenTarget::new(ForgeKind::GitHub, ""),
            TokenTarget::new(ForgeKind::GitLab, "")
        );
        // Spelling the same server differently is not (nothing is lost).
        assert_eq!(
            gitlab,
            TokenTarget::new(ForgeKind::GitLab, "  HTTPS://Git.Corp ")
        );
        // GitHub has one server: the hidden server field does not matter.
        assert_eq!(
            TokenTarget::new(ForgeKind::GitHub, ""),
            TokenTarget::new(ForgeKind::GitHub, "https://anything.example")
        );
    }

    #[test]
    fn accounts_compete_only_on_the_same_server() {
        let on = |host: &str, user: &str| {
            ForgeAccount::new(
                ForgeKind::GitLab,
                Origin::parse(host).unwrap(),
                user,
                AuthMethod::Pat,
            )
            .unwrap()
        };
        let (a, b) = (on("git.corp", "ana"), on("git.corp", "bo"));
        let (other_port, elsewhere) = (on("git.corp:8443", "ana"), on("other.corp", "ana"));
        let mut book = AccountBook::default();
        for account in [&a, &b, &other_port, &elsewhere] {
            book.add(account.clone());
        }
        assert!(shares_origin(&book, &a) && shares_origin(&book, &b));
        // Same host, other port: its own server, no "Use for" choice.
        assert!(!shares_origin(&book, &other_port));
        assert!(!shares_origin(&book, &elsewhere));
        assert_eq!(host_of(&other_port), "git.corp:8443");
    }

    #[test]
    fn closing_the_screen_stops_a_running_sign_in_and_never_undoes_a_finished_one() {
        for running in [
            SignInPhase::Requesting {
                method: SignInMethod::GitHubDevice,
                attempt: Attempt(1),
            },
            SignInPhase::AwaitingUser {
                method: SignInMethod::GitLabBrowser,
                prompt: UserPrompt::Browser {
                    url: "https://gitlab.com/oauth/authorize".into(),
                },
                interval_secs: 1,
                attempt: Attempt(2),
            },
        ] {
            assert_eq!(commands_on_close(&running), vec![SignInCommand::Abort]);
        }
        for finished in [
            SignInPhase::Idle,
            SignInPhase::Authorized(account()),
            SignInPhase::Failed(SignInFailure::Denied),
            SignInPhase::Failed(SignInFailure::Cancelled(Attempt(1))),
            SignInPhase::Failed(SignInFailure::Error("x".into())),
        ] {
            assert!(commands_on_close(&finished).is_empty(), "{finished:?}");
        }
    }

    #[test]
    fn a_cancel_that_left_something_behind_is_said_so() {
        let text = failure_text(&SignInFailure::CancelledWithLeftover("Remove it.".into()));
        assert!(text.starts_with("Cancelled, but"), "{text}");
        assert!(text.contains("Remove it."));
    }

    #[test]
    fn every_window_shares_one_service_and_a_failure_is_retried() {
        let slot = std::sync::Mutex::new(None);
        let opened = std::cell::Cell::new(0);
        let open = || {
            opened.set(opened.get() + 1);
            Ok::<_, ()>(opened.get())
        };
        assert_eq!(shared_or_open(&slot, open), Ok(1));
        assert_eq!(shared_or_open(&slot, open), Ok(1));
        assert_eq!(opened.get(), 1, "a second window opened its own copy");
        let empty: std::sync::Mutex<Option<u32>> = std::sync::Mutex::new(None);
        assert_eq!(
            shared_or_open(&empty, || Err::<u32, _>("disk")),
            Err("disk")
        );
        assert_eq!(shared_or_open(&empty, || Ok::<_, &str>(7)), Ok(7));
    }

    #[test]
    fn an_account_change_reports_what_happened() {
        let service = ForgeService::new(
            Arc::new(shell::forge::MemoryStore::new()),
            ServiceConfig::default(),
            None,
        )
        .unwrap();
        for change in [
            AccountChange::Disconnect("github|https://github.com|ghost".into()),
            AccountChange::Prefer("github|https://github.com|ghost".into()),
        ] {
            let outcome = apply_change(&service, &change);
            assert!(!outcome.changed, "{change:?}");
            assert!(
                matches!(&outcome.notice, Some((BannerKind::Error, text)) if text.contains("no longer connected")),
                "{change:?}: {outcome:?}"
            );
        }
    }

    #[test]
    fn only_web_addresses_are_handed_to_the_system() {
        for ok in [
            "https://github.com/login/device",
            "HTTPS://gitlab.com/x",
            "http://127.0.0.1:1/x",
            "http://localhost/x",
        ] {
            assert!(is_web_url(ok), "{ok}");
        }
        for bad in [
            "file:///c:/windows/system32/calc.exe",
            "javascript:alert(1)",
            "http://evil.example",
            "ms-msdt:/id",
            "calc.exe",
            "",
        ] {
            assert!(!is_web_url(bad), "{bad}");
        }
    }
}
