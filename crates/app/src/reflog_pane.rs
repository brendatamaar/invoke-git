//! Inspect a local reflog and dispatch explicitly selected recovery actions.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Task, UniformListScrollHandle, Window, div, px,
    uniform_list,
};
use invoke_core::{
    BranchName, Event, Expect, Freshness, Head, Phase, ReadId, ReflogEntry, ReflogTarget,
    ResetMode, Snapshot,
};
use shell::reflog::{ReflogEvent, list_reflog_targets, read_reflog_async};
use tokens::{ROW_LIST, SPACE_2, SPACE_3, TEXT_CODE_SMALL, TEXT_SMALL, TOOLBAR};
use ui::reflog_view::ReflogView;
use ui::scrollbar::Scrollbar;
use ui::{
    Button, CheckState, Checkbox, EditorEvent, TextEditor, TextStyleExt, ThemeExt, convert::hsla,
};

use crate::branch_panel::name_problem_text;
use crate::session::RepoSession;

enum Status {
    Loading,
    Ready,
    Failed(String),
}

enum Recovery {
    None,
    Branch { switch: bool },
    Reset { mode: ResetMode, expected: Expect },
}

pub struct ReflogPane {
    session: Entity<RepoSession>,
    editor: Entity<TextEditor>,
    target: ReflogTarget,
    targets: Arc<[ReflogTarget]>,
    show_targets: bool,
    entries: Arc<[ReflogEntry]>,
    selected: Option<usize>,
    recovery: Recovery,
    visible: bool,
    read: Option<ReadId>,
    status: Status,
    scroll: UniformListScrollHandle,
    cancel: Arc<AtomicBool>,
    task: Option<Task<()>>,
}

impl ReflogPane {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| TextEditor::new("Recovery branch name", 1, cx).single_line());
        cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| match event {
            EditorEvent::Changed => cx.notify(),
            EditorEvent::Submit if matches!(this.recovery, Recovery::Branch { .. }) => {
                this.confirm(cx)
            }
            EditorEvent::Submit => {}
        })
        .detach();
        cx.observe(&session, |this, _, cx| this.session_changed(cx))
            .detach();
        Self {
            session,
            editor,
            target: ReflogTarget::Head,
            targets: Arc::from([ReflogTarget::Head]),
            show_targets: false,
            entries: Arc::from([]),
            selected: None,
            recovery: Recovery::None,
            visible: false,
            read: None,
            status: Status::Loading,
            scroll: UniformListScrollHandle::new(),
            cancel: Arc::default(),
            task: None,
        }
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.read = self.read_id(cx);
            self.load(cx);
        } else {
            self.stop();
            self.show_targets = false;
            self.recovery = Recovery::None;
        }
    }

    fn read_id(&self, cx: &Context<Self>) -> Option<ReadId> {
        match &self.session.read(cx).state.snapshot {
            Snapshot::Read { read, .. } => Some(*read),
            Snapshot::NotRead => None,
        }
    }

    fn session_changed(&mut self, cx: &mut Context<Self>) {
        if self.visible {
            let read = self.read_id(cx);
            if self.read != read {
                self.read = read;
                self.load(cx);
            }
            cx.notify();
        }
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.task = None;
    }

    fn choose_target(&mut self, target: ReflogTarget, cx: &mut Context<Self>) {
        self.show_targets = false;
        if self.target != target {
            self.target = target;
            self.entries = Arc::from([]);
            self.scroll = UniformListScrollHandle::new();
            self.selected = None;
            self.recovery = Recovery::None;
            self.load(cx);
        }
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.stop();
        self.status = Status::Loading;
        let paths = self.session.read(cx).discovered.paths.clone();
        let target = self.target.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let worker_cancel = cancel.clone();
        let worker = cx.background_executor().spawn(async move {
            let targets = list_reflog_targets(&paths)?;
            let target = if targets.contains(&target) {
                target
            } else {
                ReflogTarget::Head
            };
            let inbox = read_reflog_async(paths, target, None, worker_cancel);
            match inbox.recv() {
                Ok(ReflogEvent::Loaded { target, entries }) => Ok((targets, target, entries)),
                Ok(ReflogEvent::Failed { error, .. }) => Err(error),
                Err(_) => Err("Reflog read stopped.".into()),
            }
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = worker.await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            this.update(cx, |this, cx| {
                match result {
                    Ok((targets, target, entries)) => {
                        if this.target != target
                            || this
                                .selected
                                .is_some_and(|ix| this.entries.get(ix) != entries.get(ix))
                        {
                            this.selected = None;
                            this.recovery = Recovery::None;
                        }
                        this.targets = targets.into();
                        this.target = target;
                        this.entries = entries.into();
                        this.status = Status::Ready;
                    }
                    Err(error) => {
                        this.status = Status::Failed(error);
                        this.selected = None;
                        this.recovery = Recovery::None;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn expect_reset(&self, cx: &Context<Self>) -> Option<Expect> {
        let state = &self.session.read(cx).state;
        let disk = state.snapshot.disk()?;
        (state.reads.freshness == Freshness::Fresh
            && disk.phase == Phase::Clean
            && !matches!(disk.head, Head::Unborn(_)))
        .then(|| Expect {
            head: disk.head.clone(),
            index: disk.index.clone(),
        })
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected.and_then(|ix| self.entries.get(ix)) else {
            return;
        };
        if !matches!(self.status, Status::Ready) {
            return;
        }
        let event = match &self.recovery {
            Recovery::None => return,
            Recovery::Branch { switch } => {
                let name = self.editor.read(cx).text().trim();
                let Ok(name) = BranchName::parse(name) else {
                    return;
                };
                entry.create_branch_event(name.0, *switch)
            }
            Recovery::Reset { mode, expected } => {
                if self.expect_reset(cx).as_ref() != Some(expected) {
                    return;
                }
                entry.reset_event(*mode)
            }
        };
        self.recovery = Recovery::None;
        self.session.update(cx, |session, cx| {
            session.deliver(Event::NoticeDismissed, cx);
            session.deliver(event, cx);
        });
        cx.notify();
    }

    fn recovery_form(&self, entry: &ReflogEntry, cx: &mut Context<Self>) -> gpui::Div {
        let p = cx.palette();
        let cancel = cx.entity();
        let close = Button::new("reflog-recovery-cancel", "Cancel")
            .ghost()
            .on_click(move |_, _, cx| {
                cancel.update(cx, |this, cx| {
                    this.recovery = Recovery::None;
                    cx.notify();
                })
            });
        let confirm = cx.entity();
        let ready = matches!(self.status, Status::Ready);
        match &self.recovery {
            Recovery::None => {
                let branch = cx.entity();
                let reset_available = ready && self.expect_reset(cx).is_some();
                let reset = |id: &'static str, label: &'static str, mode| {
                    let pane = cx.entity();
                    Button::new(id, label)
                        .disabled(!reset_available)
                        .on_click(move |_, _, cx| {
                            pane.update(cx, |this, cx| {
                                if let Some(expected) = this.expect_reset(cx) {
                                    this.recovery = Recovery::Reset { mode, expected };
                                    cx.notify();
                                }
                            });
                        })
                };
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(SPACE_2))
                    .child(
                        Button::new("reflog-branch", "Create branch at this commit")
                            .disabled(!ready)
                            .on_click(move |_, _, cx| {
                                branch.update(cx, |this, cx| {
                                    this.editor.update(cx, |editor, cx| editor.set_text("", cx));
                                    this.recovery = Recovery::Branch { switch: false };
                                    cx.notify();
                                })
                            }),
                    )
                    .child(reset("reflog-reset-soft", "Soft reset…", ResetMode::Soft))
                    .child(reset(
                        "reflog-reset-mixed",
                        "Mixed reset…",
                        ResetMode::Mixed,
                    ))
            }
            Recovery::Branch { switch } => {
                let toggle = cx.entity();
                let problem = BranchName::parse(self.editor.read(cx).text().trim()).err();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_2))
                    .child(self.editor.clone())
                    .children(
                        problem
                            .filter(|_| !self.editor.read(cx).text().is_empty())
                            .map(|problem| {
                                div()
                                    .text_color(hsla(p.danger))
                                    .child(name_problem_text(problem))
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(SPACE_2))
                            .child(
                                Checkbox::new(
                                    "reflog-switch",
                                    if *switch {
                                        CheckState::Checked
                                    } else {
                                        CheckState::Unchecked
                                    },
                                )
                                .on_click(move |_, _, cx| {
                                    toggle.update(cx, |this, cx| {
                                        if let Recovery::Branch { switch } = &mut this.recovery {
                                            *switch = !*switch;
                                            cx.notify();
                                        }
                                    })
                                }),
                            )
                            .child("Switch to the recovery branch"),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(SPACE_2))
                            .child(
                                Button::new("reflog-create-branch", "Create branch")
                                    .primary()
                                    .disabled(!ready || problem.is_some())
                                    .on_click(move |_, _, cx| {
                                        confirm.update(cx, |this, cx| this.confirm(cx))
                                    }),
                            )
                            .child(close),
                    )
            }
            Recovery::Reset { mode, expected } => {
                let current = self.expect_reset(cx).as_ref() == Some(expected);
                let destination = match &expected.head {
                    Head::Branch { name, .. } => name.0.as_str(),
                    Head::Detached(_) | Head::Unborn(_) => "HEAD",
                };
                let (label, details) = match mode {
                    ResetMode::Soft => (
                        "Soft reset",
                        "Moves the current ref and keeps the index and working files.",
                    ),
                    ResetMode::Mixed => (
                        "Mixed reset",
                        "Moves the current ref and resets the index. Working files are kept.",
                    ),
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_2))
                    .child(format!("{label} {destination} to {}?", entry.new_oid))
                    .child(details)
                    .children((!current && ready).then(|| {
                        div().text_color(hsla(p.danger)).child(
                            "The current ref or index changed. Reopen the reset confirmation.",
                        )
                    }))
                    .child(
                        div()
                            .flex()
                            .gap(px(SPACE_2))
                            .child(
                                Button::new("reflog-confirm-reset", label)
                                    .danger()
                                    .disabled(!ready || !current)
                                    .on_click(move |_, _, cx| {
                                        confirm.update(cx, |this, cx| this.confirm(cx))
                                    }),
                            )
                            .child(close),
                    )
            }
        }
    }
}

impl Drop for ReflogPane {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Render for ReflogPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let picker = cx.entity();
        let targets_scroll = window
            .use_keyed_state("reflog-targets-scroll", cx, |_, _| {
                UniformListScrollHandle::new()
            })
            .read(cx)
            .clone();
        let targets = self.show_targets.then(|| {
            let targets = self.targets.clone();
            let count = targets.len();
            let choose = cx.entity();
            div()
                .flex()
                .flex_none()
                .h(px(count.min(8) as f32 * ROW_LIST))
                .bg(hsla(p.bg_raised))
                .border_b_1()
                .border_color(hsla(p.line))
                .child(
                    uniform_list("reflog-targets", count, move |range, _, _| {
                        range
                            .map(|ix| {
                                let target = targets[ix].clone();
                                let choose = choose.clone();
                                div()
                                    .flex()
                                    .items_center()
                                    .h(px(ROW_LIST))
                                    .px(px(SPACE_2))
                                    .child(
                                        Button::new(
                                            ("reflog-target", ix),
                                            target.label().to_owned(),
                                        )
                                        .ghost()
                                        .on_click(
                                            move |_, _, cx| {
                                                choose.update(cx, |this, cx| {
                                                    this.choose_target(target.clone(), cx)
                                                })
                                            },
                                        ),
                                    )
                            })
                            .collect()
                    })
                    .track_scroll(&targets_scroll)
                    .flex_1()
                    .h_full(),
                )
                .child(Scrollbar::new(
                    "reflog-targets-scrollbar",
                    targets_scroll,
                    count,
                    ROW_LIST,
                ))
        });
        let selected = self.selected.and_then(|ix| self.entries.get(ix));
        let details = selected.map(|entry| {
            let from = entry.old_oid.as_ref().map_or("—", |oid| oid.as_hex());
            div()
                .flex()
                .flex_none()
                .flex_col()
                .gap(px(SPACE_2))
                .p(px(SPACE_3))
                .border_t_1()
                .border_color(hsla(p.line))
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink))
                .child(
                    div()
                        .id("reflog-entry-message")
                        .max_h(px(ROW_LIST * 3.))
                        .overflow_y_scroll()
                        .child(format!("{} · {}", entry.selector, entry.message)),
                )
                .child(
                    div()
                        .text_style(&TEXT_CODE_SMALL, cx)
                        .child(format!("Ref moved from {from} to {}", entry.new_oid)),
                )
                .child(self.recovery_form(entry, cx))
        });
        let view = cx.entity();
        let entries = self.entries.clone();
        let list = ReflogView::new(
            entries.clone(),
            self.selected,
            self.scroll.clone(),
            move |ix, _, cx| {
                view.update(cx, |this, cx| {
                    if Arc::ptr_eq(&this.entries, &entries) {
                        this.selected = Some(ix);
                        this.recovery = Recovery::None;
                        cx.notify();
                    }
                });
            },
        );
        let message = match &self.status {
            Status::Loading => Some("Reading reflog…"),
            Status::Ready if self.entries.is_empty() => Some("No recorded movements for this ref."),
            Status::Ready if self.selected.is_none() => {
                Some("Select an entry to recover its resulting commit.")
            }
            Status::Ready => None,
            Status::Failed(error) => Some(error.as_str()),
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.))
            .bg(hsla(p.bg_panel))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(SPACE_2))
                    .h(px(TOOLBAR))
                    .px(px(SPACE_3))
                    .border_b_1()
                    .border_color(hsla(p.line))
                    .child(
                        Button::new(
                            "reflog-target-picker",
                            format!("Reflog for {} ▾", self.target.label()),
                        )
                        .on_click(move |_, _, cx| {
                            picker.update(cx, |this, cx| {
                                this.show_targets = !this.show_targets;
                                cx.notify();
                            })
                        }),
                    )
                    .children(message.map(|text| {
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(text.to_owned())
                    })),
            )
            .children(targets)
            .child(div().flex_1().min_h(px(0.)).py(px(SPACE_2)).child(list))
            .children(details)
    }
}
