//! Fetch, pull and push on screen: the buttons in the header of a repo tab,
//! and the strip under it that shows a network operation in progress (with
//! Cancel), asks for credentials in the app, and offers the next step after
//! a rejected push. The decisions are in `sync`; the work is the core's.

use gpui::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Subscription, Window, deferred, div, px,
};
use invoke_core::{Event, Notice, OpId, OpPhase, PullMode, RunningOp};
use tokens::{RADIUS_LG, ROW_LIST, SIDEBAR, SPACE_1, SPACE_2, SPACE_3, SPACE_8, TEXT_SMALL};
use ui::convert::{hsla, shadows};
use ui::{Banner, BannerKind, Button, IconName, ProgressBar, TextStyleExt, ThemeExt};

use crate::auth_prompt::{AuthEvent, AuthPromptView};
use crate::session::RepoSession;
use crate::sync::{self, Plan};

fn plan_of(session: &Entity<RepoSession>, cx: &gpui::App) -> Option<Plan> {
    sync::plan(&session.read(cx).state)
}

fn deliver(session: &Entity<RepoSession>, event: Event, cx: &mut gpui::App) {
    session.update(cx, |session, cx| session.deliver(event, cx));
}

/// What the buttons ask the tab to do besides running git.
pub enum SyncEvent {
    /// Open the pre-push review (task 21); the review has the Push button.
    Review,
}

/// Fetch, Pull and Push for the branch that is checked out. Push opens the
/// review first.
pub struct SyncButtons {
    session: Entity<RepoSession>,
    pull_menu_open: bool,
    _observe: Subscription,
}

impl EventEmitter<SyncEvent> for SyncButtons {}

impl SyncButtons {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&session, |this, _, cx| {
            this.pull_menu_open = false;
            cx.notify();
        });
        Self {
            session,
            pull_menu_open: false,
            _observe: observe,
        }
    }
}

impl Render for SyncButtons {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let busy = self.session.read(cx).state.ops.running.is_some();
        let plan = plan_of(&self.session, cx);
        // A paused operation owns the repo: the core refuses these until it ends.
        if plan.as_ref().is_some_and(|plan| !plan.clean) {
            return div()
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink_muted))
                .child("Fetch, pull and push are paused until this is finished")
                .into_any_element();
        }
        let ready = plan.is_some() && !busy;
        let (can_pull, can_push, diverged) = plan
            .as_ref()
            .map(|plan| (plan.can_pull(), plan.can_push(), plan.diverged()))
            .unwrap_or_default();
        let behind = plan.as_ref().map_or(0, Plan::behind);
        let ahead = plan.as_ref().map_or(0, Plan::ahead);
        let publishes = plan.as_ref().is_some_and(Plan::publishes);
        let pull_label = if behind > 0 {
            format!("Pull ↓{behind}")
        } else {
            "Pull".to_owned()
        };
        let push_label = if publishes && can_push {
            "Review & publish".to_owned()
        } else if ahead > 0 {
            format!("Review & push ↑{ahead}")
        } else {
            "Push".to_owned()
        };
        let run = |plan: &Option<Plan>, make: fn(&Plan) -> Option<Event>| {
            let plan = plan.clone();
            move |session: &Entity<RepoSession>, cx: &mut gpui::App| {
                if let Some(event) = plan.as_ref().and_then(make) {
                    deliver(session, event, cx);
                }
            }
        };
        let (fetch, pull, pull_merge, rebase) = (
            run(&plan, |plan| Some(plan.fetch())),
            run(&plan, |plan| plan.pull(PullMode::Merge)),
            run(&plan, |plan| plan.pull(PullMode::Merge)),
            run(&plan, |plan| plan.pull(PullMode::Rebase)),
        );
        let (s1, s2, s3, s4) = (
            self.session.clone(),
            self.session.clone(),
            self.session.clone(),
            self.session.clone(),
        );
        let review = cx.entity();
        let push_is_primary = ready && can_push && (ahead > 0 || publishes);
        let pull_is_primary = ready && can_pull && behind > 0 && !push_is_primary;

        let mut pull_btn = Button::new("sync-pull", pull_label)
            .icon(IconName::Pull)
            .disabled(!ready || !can_pull)
            .on_click(move |_, _, cx| pull(&s2, cx));
        if pull_is_primary {
            pull_btn = pull_btn.primary();
        }

        let mut pull_group = div()
            .relative()
            .flex()
            .items_center()
            .gap(px(SPACE_1 / 2.0))
            .child(pull_btn);
        if diverged {
            let toggle = cx.entity();
            let close_out = cx.entity();
            let close_merge = cx.entity();
            let close_rebase = cx.entity();
            pull_group = pull_group
                .child(
                    Button::new("sync-pull-more", "")
                        .icon(IconName::ChevronDown)
                        .disabled(!ready || !can_pull)
                        .on_click(move |_, _, cx| {
                            toggle.update(cx, |this, cx| {
                                this.pull_menu_open = !this.pull_menu_open;
                                cx.notify();
                            })
                        }),
                )
                .children((self.pull_menu_open && ready && can_pull).then(|| {
                    deferred(
                        div()
                            .id("sync-pull-popover")
                            .occlude()
                            .absolute()
                            .top(px(ROW_LIST + SPACE_1))
                            .right(px(0.))
                            .flex()
                            .flex_col()
                            .min_w(px(SIDEBAR - SPACE_8 * 2.0))
                            .gap(px(SPACE_1 / 2.0))
                            .p(px(SPACE_1))
                            .rounded(px(RADIUS_LG))
                            .border_1()
                            .border_color(hsla(p.line))
                            .bg(hsla(p.bg_raised))
                            .shadow(shadows(p.shadow_overlay))
                            .on_mouse_down_out(move |_, _, cx| {
                                cx.stop_propagation();
                                close_out.update(cx, |this, cx| {
                                    this.pull_menu_open = false;
                                    cx.notify();
                                });
                            })
                            .child(
                                Button::new("sync-pull-merge", "Pull (merge)")
                                    .icon(IconName::Pull)
                                    .ghost()
                                    .full_width()
                                    .on_click(move |_, _, cx| {
                                        close_merge.update(cx, |this, cx| {
                                            this.pull_menu_open = false;
                                            cx.notify();
                                        });
                                        pull_merge(&s3, cx);
                                    }),
                            )
                            .child(
                                Button::new("sync-rebase", "Pull with rebase")
                                    .icon(IconName::Branch)
                                    .ghost()
                                    .full_width()
                                    .on_click(move |_, _, cx| {
                                        close_rebase.update(cx, |this, cx| {
                                            this.pull_menu_open = false;
                                            cx.notify();
                                        });
                                        rebase(&s4, cx);
                                    }),
                            ),
                    )
                }));
        }

        let mut push_btn = Button::new("sync-push", push_label)
            .icon(IconName::Push)
            .disabled(!ready || !can_push)
            .on_click(move |_, _, cx| review.update(cx, |_, cx| cx.emit(SyncEvent::Review)));
        if push_is_primary {
            push_btn = push_btn.primary();
        }

        div()
            .flex()
            .items_center()
            .gap(px(SPACE_2))
            .child(
                Button::new("sync-fetch", "Fetch")
                    .icon(IconName::Fetch)
                    .disabled(!ready)
                    .on_click(move |_, _, cx| fetch(&s1, cx)),
            )
            .child(pull_group)
            .child(push_btn)
            .into_any_element()
    }
}

/// The strip under the header: a network operation, its credential question,
/// the choices after a rejected push.
pub struct SyncStrip {
    session: Entity<RepoSession>,
    /// The question on screen, with the operation and the prompt it is for.
    auth: Option<(OpId, invoke_core::AuthPrompt, Entity<AuthPromptView>)>,
    focus_prompt: bool,
    /// The user pressed "Force push…" and has not answered yet.
    confirm_force: bool,
    /// The user pressed "Remove lock file…" and has not answered yet.
    confirm_lock: bool,
    _observe: Subscription,
}

impl SyncStrip {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&session, |this, _, cx| this.session_changed(cx));
        Self {
            session,
            auth: None,
            focus_prompt: false,
            confirm_force: false,
            confirm_lock: false,
            _observe: observe,
        }
    }

    fn rejected(&self, cx: &Context<Self>) -> bool {
        matches!(
            self.session.read(cx).state.notice,
            Some(Notice::PushRejected { .. })
        )
    }

    fn session_changed(&mut self, cx: &mut Context<Self>) {
        if !self.rejected(cx) {
            self.confirm_force = false;
        }
        if !sync::lock_to_offer(&self.session.read(cx).state) {
            self.confirm_lock = false;
        }
        let wanted = match &self.session.read(cx).state.ops.running {
            Some(RunningOp {
                id,
                phase: OpPhase::AwaitingAuth(prompt),
                ..
            }) => Some((*id, prompt.clone())),
            _ => None,
        };
        match wanted {
            Some((op, prompt)) => {
                let same = self
                    .auth
                    .as_ref()
                    .is_some_and(|(have, asked, _)| *have == op && *asked == prompt);
                if !same {
                    let view = cx.new(|cx| AuthPromptView::new(prompt.clone(), cx));
                    cx.subscribe(&view, move |this, _, event: &AuthEvent, cx| {
                        this.auth = None;
                        let event = match event {
                            AuthEvent::Answer(answer) => Event::AuthAnswered(op, answer.clone()),
                            AuthEvent::Cancel => Event::OpCancelRequested(op),
                        };
                        deliver(&this.session, event, cx);
                    })
                    .detach();
                    self.auth = Some((op, prompt, view));
                    self.focus_prompt = true;
                }
            }
            None => self.auth = None,
        }
        cx.notify();
    }

    fn send(&mut self, event: Option<Event>, cx: &mut Context<Self>) {
        self.confirm_force = false;
        if let Some(event) = event {
            deliver(&self.session, event, cx);
        }
        cx.notify();
    }
}

impl Render for SyncStrip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        if std::mem::take(&mut self.focus_prompt)
            && let Some((_, _, view)) = self.auth.clone()
        {
            view.update(cx, |view, cx| view.focus(window, cx));
        }
        let state = &self.session.read(cx).state;
        let plan = sync::plan(state);
        let mut column = div().flex().flex_col().gap(px(SPACE_2));
        let mut any = false;

        if let Some(running) = &state.ops.running
            && let Some(what) = sync::what(&running.request)
        {
            any = true;
            let op = running.id;
            match &running.phase {
                OpPhase::AwaitingAuth(_) => {
                    if let Some((_, _, view)) = &self.auth {
                        column = column
                            .child(
                                div()
                                    .text_style(&TEXT_SMALL, cx)
                                    .text_color(hsla(p.ink_muted))
                                    .child(what),
                            )
                            .child(div().max_w(px(460.)).child(view.clone()));
                    }
                }
                OpPhase::Cancelling => {
                    column = column.child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(format!("{what}: cancelling…")),
                    );
                }
                OpPhase::Running { .. } => {
                    let (stage, fraction) = sync::progress(running);
                    let cancel = cx.entity();
                    column = column.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(SPACE_3))
                            .child(
                                div()
                                    .flex_none()
                                    .text_style(&TEXT_SMALL, cx)
                                    .text_color(hsla(p.ink))
                                    .child(what),
                            )
                            .children(fraction.map(|fraction| {
                                div()
                                    .w(px(tokens::SIDEBAR))
                                    .child(ProgressBar::new(fraction))
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_style(&TEXT_SMALL, cx)
                                    .text_color(hsla(p.ink_muted))
                                    .child(match fraction {
                                        Some(fraction) => {
                                            format!("{stage}: {:.0}%", fraction * 100.0)
                                        }
                                        None => stage,
                                    }),
                            )
                            .child(Button::new("sync-cancel", "Cancel").on_click(
                                move |_, _, cx| {
                                    cancel.update(cx, |this, cx| {
                                        this.send(Some(Event::OpCancelRequested(op)), cx)
                                    })
                                },
                            )),
                    );
                }
            }
        }

        if let Some(Notice::PushRejected { failure, .. }) = &state.notice {
            any = true;
            let details = crate::notice::headline(&failure.details);
            let target = plan.as_ref().and_then(Plan::push_target);
            if self.confirm_force {
                let (yes, no) = (cx.entity(), cx.entity());
                let remote = target.as_ref().map(sync::remote_text).unwrap_or_default();
                let force = plan.as_ref().and_then(|plan| plan.push(true));
                column = column.child(
                    Banner::new(
                        BannerKind::Warning,
                        format!(
                            "Force push replaces {remote} with your branch, but only if nobody has pushed there since you last fetched. Commits on the remote that you do not have are lost."
                        ),
                    )
                    .action(
                        Button::new("force-yes", "Force push")
                            .danger()
                            .disabled(force.is_none())
                            .on_click(move |_, _, cx| {
                                yes.update(cx, |this, cx| this.send(force.clone(), cx))
                            }),
                    )
                    .action(
                        Button::new("force-no", "Cancel")
                            .ghost()
                            .on_click(move |_, _, cx| no.update(cx, |this, cx| this.send(None, cx))),
                    ),
                );
            } else {
                let (fetch, rebase, force, dismiss) =
                    (cx.entity(), cx.entity(), cx.entity(), cx.entity());
                let fetch_event = plan.as_ref().map(Plan::fetch);
                let rebase_event = plan.as_ref().and_then(|plan| plan.pull(PullMode::Rebase));
                let can_rebase = rebase_event.is_some();
                column = column.child(
                    Banner::new(
                        BannerKind::Warning,
                        "Push rejected: the remote has new commits. Fetch and rebase to continue.",
                    )
                    .action(
                        Button::new("rejected-fetch", "Fetch").on_click(move |_, _, cx| {
                            fetch.update(cx, |this, cx| this.send(fetch_event.clone(), cx))
                        }),
                    )
                    .action(
                        Button::new("rejected-rebase", "Pull with rebase")
                            .disabled(!can_rebase)
                            .on_click(move |_, _, cx| {
                                rebase.update(cx, |this, cx| this.send(rebase_event.clone(), cx))
                            }),
                    )
                    .action(
                        Button::new("rejected-force", "Force push…")
                            .ghost()
                            .on_click(move |_, _, cx| {
                                force.update(cx, |this, cx| {
                                    this.confirm_force = true;
                                    cx.notify();
                                })
                            }),
                    )
                    .action(Button::new("rejected-dismiss", "Dismiss").ghost().on_click(
                        move |_, _, cx| {
                            dismiss
                                .update(cx, |this, cx| this.send(Some(Event::NoticeDismissed), cx))
                        },
                    ))
                    .action(
                        div()
                            .max_w(px(320.))
                            .truncate()
                            .text_style(&TEXT_SMALL, cx)
                            .child(details),
                    ),
                );
            }
        }

        if sync::lock_to_offer(state) {
            any = true;
            let (ask, yes, no) = (cx.entity(), cx.entity(), cx.entity());
            column = column.child(if self.confirm_lock {
                Banner::new(
                    BannerKind::Warning,
                    "Remove the lock file? Do this only if no other git program is working in this repository. If one is, it may be damaged.",
                )
                .action(
                    Button::new("lock-yes", "Remove lock file")
                        .danger()
                        .on_click(move |_, _, cx| {
                            yes.update(cx, |this, cx| {
                                this.confirm_lock = false;
                                this.send(Some(Event::RemoveStaleLockRequested), cx)
                            })
                        }),
                )
                .action(Button::new("lock-no", "Cancel").ghost().on_click(
                    move |_, _, cx| {
                        no.update(cx, |this, cx| {
                            this.confirm_lock = false;
                            cx.notify();
                        })
                    },
                ))
            } else {
                Banner::new(
                    BannerKind::Warning,
                    "Another git process is running in this repository, so changes wait. If a git program was cancelled or crashed, its lock file may be left over.",
                )
                .action(
                    Button::new("lock-ask", "Remove lock file…")
                        .ghost()
                        .on_click(move |_, _, cx| {
                            ask.update(cx, |this, cx| {
                                this.confirm_lock = true;
                                cx.notify();
                            })
                        }),
                )
            });
        }

        if any {
            div()
                .p(px(SPACE_2))
                .bg(hsla(p.bg_panel))
                .border_b_1()
                .border_color(hsla(p.line))
                .child(column)
        } else {
            div()
        }
    }
}
