//! The commit message box under the status tree: the editor, the amend
//! checkbox, the subject length, and the Commit button. It sends
//! `CommitRequested`; the core decides whether the commit can run and says
//! why not in a notice (shown by the repo view).

use gpui::{
    AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, Styled,
    Subscription, Task, Window, div, px,
};
use invoke_core::message::{SubjectLevel, subject_hint};
use invoke_core::{
    Action, CommitMessage, CommitRefusal, Event, Freshness, Head, LockState, MessageProblem,
    Notice, Oid, OpId, OpOutcome, Phase, Refusal, Snapshot, commit,
};
use shell::{head_message, merge_message};
use tokens::{SPACE_2, SPACE_3, TEXT_SMALL};
use ui::convert::hsla;
use ui::{
    Button, CheckState, Checkbox, EditorEvent, SectionLabel, TextEditor, TextStyleExt, ThemeExt,
};

use crate::session::RepoSession;

/// The Commit button's words and whether it can be pressed.
///
/// A plain commit needs a message and something staged; an amend needs only
/// a message (it can reword). The core refuses what is not allowed; this
/// only keeps an obviously empty box from looking ready.
pub fn commit_button(staged: usize, amend: bool, message_empty: bool) -> (String, bool) {
    if amend {
        return ("Amend commit".to_owned(), !message_empty);
    }
    let files = match staged {
        0 => "no staged files".to_owned(),
        1 => "1 staged file".to_owned(),
        n => format!("{n} staged files"),
    };
    (format!("Commit {files}"), !message_empty && staged > 0)
}

pub struct CommitBox {
    session: Entity<RepoSession>,
    editor: Entity<TextEditor>,
    amend_target: Option<Oid>,
    amend_load: Option<Task<()>>,
    /// What was typed before Amend filled the box with the last message.
    draft: Option<String>,
    /// A commit was sent and its outcome is not known yet.
    pending: Option<(OpId, String)>,
    /// The merge message was offered once for this merge.
    merge_offered: bool,
    _subscriptions: Vec<Subscription>,
}

pub struct CommitSucceeded;

impl EventEmitter<CommitSucceeded> for CommitBox {}

impl CommitBox {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| TextEditor::new("Commit message (subject, then details)", 4, cx));
        let subscriptions = vec![
            cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| match event {
                EditorEvent::Changed => cx.notify(),
                EditorEvent::Submit => this.commit(cx),
            }),
            cx.observe(&session, |this, _, cx| this.session_changed(cx)),
        ];
        Self {
            session,
            editor,
            amend_target: None,
            amend_load: None,
            draft: None,
            pending: None,
            merge_offered: false,
            _subscriptions: subscriptions,
        }
    }

    /// The regular draft, including while the amend message is in the editor.
    pub fn draft(&self, cx: &gpui::App) -> String {
        self.draft
            .clone()
            .unwrap_or_else(|| self.editor.read(cx).text().to_owned())
    }

    pub fn set_draft(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.draft(cx) == text {
            return;
        }
        if let Some(draft) = &mut self.draft {
            *draft = text.to_owned();
            cx.notify();
        } else {
            self.editor
                .update(cx, |editor, cx| editor.set_text(text, cx));
        }
    }

    fn head_oid(&self, cx: &Context<Self>) -> Option<Oid> {
        match &self.session.read(cx).state.snapshot.disk()?.head {
            Head::Branch { oid, .. } | Head::Detached(oid) => Some(oid.clone()),
            Head::Unborn(_) => None,
        }
    }

    fn readiness(&self, text: &str, cx: &Context<Self>) -> Result<(), &'static str> {
        let state = &self.session.read(cx).state;
        let disk = state.snapshot.disk().ok_or("Reading repository status…")?;
        if self.pending.is_some() {
            return Err("Commit in progress…");
        }
        if disk.lock == LockState::HeldByOther {
            return Err("Another Git process holds the index lock.");
        }
        if self.amend_target.is_some() {
            if self.amend_target != self.head_oid(cx) {
                return Err("HEAD changed. Turn off Amend and select the new target.");
            }
            if state.ops.running.is_some()
                || !state.ops.queued.is_empty()
                || state.reads.freshness != Freshness::Fresh
            {
                return Err("Wait for Git to refresh HEAD before amending.");
            }
        }
        commit::plan(disk, self.amend_target.is_some()).map_err(|reason| match reason {
            Refusal::Commit(CommitRefusal::NothingStaged) => "Stage changes to commit.",
            Refusal::Commit(CommitRefusal::UnresolvedConflicts) => "Resolve conflicts to commit.",
            Refusal::Commit(CommitRefusal::AmendDuringMerge) => "Finish the merge before amending.",
            Refusal::Commit(CommitRefusal::AmendUnborn) => "There is no commit to amend yet.",
            _ => "Use the controls for the Git operation in progress.",
        })?;
        CommitMessage::parse(text).map_err(|problem| match problem {
            MessageProblem::ContainsNul => "Remove the NUL character from the commit message.",
            MessageProblem::Empty | MessageProblem::OnlyWhitespace => "Enter a commit message.",
        })?;
        Ok(())
    }

    fn staged_count(&self, cx: &Context<Self>) -> usize {
        match &self.session.read(cx).state.snapshot {
            Snapshot::Read { disk, .. } => disk.status.staged().count(),
            Snapshot::NotRead => 0,
        }
    }

    /// Reacts to the session: a commit that succeeded clears the box; a
    /// merge in progress offers git's own message once.
    fn session_changed(&mut self, cx: &mut Context<Self>) {
        let (notice, merging) = {
            let state = &self.session.read(cx).state;
            (
                state.notice.clone(),
                matches!(
                    &state.snapshot,
                    Snapshot::Read { disk, .. } if matches!(disk.phase, Phase::Merging { .. })
                ),
            )
        };
        if let Some((id, submitted)) = &self.pending {
            match notice {
                Some(Notice::OpEnded { op, outcome }) if op == *id => {
                    let unchanged = self.editor.read(cx).text() == submitted.as_str();
                    self.pending = None;
                    if outcome == OpOutcome::Succeeded {
                        cx.emit(CommitSucceeded);
                        if self.amend_target.is_some() {
                            self.set_amend(false, cx);
                        } else if unchanged {
                            self.set_draft("", cx);
                        }
                    }
                }
                Some(Notice::Refused {
                    action: Action::Commit,
                    ..
                }) => self.pending = None,
                _ => {}
            }
        }
        if !merging {
            self.merge_offered = false;
        } else if !self.merge_offered {
            self.merge_offered = true;
            if self.editor.read(cx).is_empty() {
                let paths = self.session.read(cx).discovered.paths.clone();
                let editor = self.editor.clone();
                cx.spawn(async move |_, cx| {
                    let message = cx
                        .background_executor()
                        .spawn(async move { merge_message(&paths) })
                        .await;
                    if let Ok(Some(text)) = message {
                        editor.update(cx, |editor, cx| {
                            if editor.is_empty() {
                                editor.set_text(&text, cx);
                            }
                        });
                    }
                })
                .detach();
            }
        }
        cx.notify();
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        let message = self.editor.read(cx).text().to_owned();
        if self.readiness(&message, cx).is_err() {
            return;
        }
        self.pending = Some((self.session.read(cx).state.ops.next, message.clone()));
        let amend = self.amend_target.is_some();
        self.session.update(cx, |session, cx| {
            session.deliver(Event::NoticeDismissed, cx);
            session.deliver(Event::CommitRequested { message, amend }, cx);
        });
    }

    /// Ticking Amend fills the box with the last commit's message; unticking
    /// brings back what was typed before.
    fn set_amend(&mut self, amend: bool, cx: &mut Context<Self>) {
        if amend == self.amend_target.is_some() || self.pending.is_some() {
            return;
        }
        if amend {
            let Some(target) = self.head_oid(cx) else {
                return;
            };
            self.amend_target = Some(target.clone());
            self.draft = Some(self.draft(cx));
            self.editor.update(cx, |editor, cx| editor.set_text("", cx));
            let paths = self.session.read(cx).discovered.paths.clone();
            self.amend_load = Some(cx.spawn(async move |this, cx| {
                let message = cx
                    .background_executor()
                    .spawn(async move { head_message(&paths) })
                    .await;
                if let Ok(Some(text)) = message {
                    this.update(cx, |this, cx| {
                        if this.head_oid(cx).as_ref() == Some(&target)
                            && this.editor.read(cx).is_empty()
                            && this.pending.is_none()
                        {
                            this.editor
                                .update(cx, |editor, cx| editor.set_text(&text, cx));
                        }
                    })
                    .ok();
                }
            }));
        } else {
            self.amend_load = None;
            self.amend_target = None;
            if let Some(draft) = self.draft.take() {
                self.set_draft(&draft, cx);
            }
        }
        cx.notify();
    }
}

impl Render for CommitBox {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let text = self.editor.read(cx).text().to_owned();
        let hint = subject_hint(&text);
        let disk = self.session.read(cx).state.snapshot.disk();
        let unstaged = disk.map_or(0, |disk| disk.status.unstaged().count());
        let staged = self.staged_count(cx);
        let scope = format!("{staged} staged · {unstaged} unstaged");
        let amend = self.amend_target.is_some();
        let readiness = self.readiness(&text, cx);
        let merging = disk.is_some_and(|disk| matches!(disk.phase, Phase::Merging { .. }));
        let amend_label = self
            .amend_target
            .clone()
            .or_else(|| self.head_oid(cx))
            .map(|oid| format!("Amend {}", ui::short_hash(oid.as_hex())));
        let can_toggle_amend = self.pending.is_none()
            && (amend
                || (amend_label.is_some()
                    && disk.is_some_and(|disk| commit::plan(disk, true).is_ok())));
        let (label, _) = commit_button(staged, amend, text.trim().is_empty());
        let label = if merging && !amend {
            "Commit merge".into()
        } else {
            label
        };
        let counter_ink = match hint.level {
            SubjectLevel::Fine => p.ink_faint,
            SubjectLevel::PastSoft => p.warning,
            SubjectLevel::PastLimit => p.danger,
        };
        let (check, commit) = (cx.entity(), cx.entity());
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(SPACE_2))
            .p(px(SPACE_3))
            .border_t_1()
            .border_color(hsla(p.line))
            .child(SectionLabel::new("Commit message"))
            .child(
                div()
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(scope),
            )
            .child(self.editor.clone())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(SPACE_2))
                    .children(can_toggle_amend.then(|| {
                        Checkbox::new(
                            "amend",
                            if amend {
                                CheckState::Checked
                            } else {
                                CheckState::Unchecked
                            },
                        )
                        .label(amend_label.unwrap_or_else(|| "Amend last commit".into()))
                        .on_click(move |_, _, cx| {
                            check.update(cx, |this, cx| {
                                let next = this.amend_target.is_none();
                                this.set_amend(next, cx)
                            })
                        })
                    }))
                    .child(
                        div()
                            .flex_none()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(counter_ink))
                            .child(format!("Subject {} / 72", hint.chars)),
                    ),
            )
            .children(readiness.as_ref().err().map(|reason| {
                div()
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(*reason)
            }))
            .child(
                Button::new("commit", label)
                    .primary()
                    .full_width()
                    .disabled(readiness.is_err())
                    .on_click(move |_, _, cx| commit.update(cx, |this, cx| this.commit(cx))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_button_names_the_staged_files() {
        assert_eq!(
            commit_button(1, false, false),
            ("Commit 1 staged file".to_owned(), true)
        );
        assert_eq!(
            commit_button(3, false, false),
            ("Commit 3 staged files".to_owned(), true)
        );
    }

    #[test]
    fn an_empty_message_or_nothing_staged_is_not_ready() {
        assert!(!commit_button(2, false, true).1);
        assert!(!commit_button(0, false, false).1);
    }

    #[test]
    fn an_amend_needs_only_a_message() {
        assert_eq!(
            commit_button(0, true, false),
            ("Amend commit".to_owned(), true)
        );
        assert!(!commit_button(0, true, true).1);
    }
}
