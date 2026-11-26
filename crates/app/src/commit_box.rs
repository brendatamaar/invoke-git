//! The commit message box under the status tree: the editor, the amend
//! checkbox, the subject length, and the Commit button. It sends
//! `CommitRequested`; the core decides whether the commit can run and says
//! why not in a notice (shown by the repo view).

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window,
    div, px,
};
use invoke_core::message::{SubjectLevel, subject_hint};
use invoke_core::{Event, Head, Notice, OpOutcome, Phase, Snapshot};
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
pub fn commit_button(
    staged: usize,
    branch: &str,
    amend: bool,
    message_empty: bool,
) -> (String, bool) {
    if amend {
        return ("Amend commit".to_owned(), !message_empty);
    }
    let files = match staged {
        0 => "no files".to_owned(),
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    };
    (
        format!("Commit {files} to {branch}"),
        !message_empty && staged > 0,
    )
}

pub struct CommitBox {
    session: Entity<RepoSession>,
    editor: Entity<TextEditor>,
    amend: bool,
    /// What was typed before Amend filled the box with the last message.
    draft: Option<String>,
    /// A commit was sent and its outcome is not known yet.
    pending: bool,
    /// The merge message was offered once for this merge.
    merge_offered: bool,
    _subscriptions: Vec<Subscription>,
}

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
            amend: false,
            draft: None,
            pending: false,
            merge_offered: false,
            _subscriptions: subscriptions,
        }
    }

    fn staged_count(&self, cx: &Context<Self>) -> usize {
        match &self.session.read(cx).state.snapshot {
            Snapshot::Read { disk, .. } => disk.status.staged().count(),
            Snapshot::NotRead => 0,
        }
    }

    fn branch_name(&self, cx: &Context<Self>) -> String {
        match &self.session.read(cx).state.snapshot {
            Snapshot::Read { disk, .. } => match &disk.head {
                Head::Branch { name, .. } | Head::Unborn(name) => name.0.clone(),
                Head::Detached(_) => "HEAD".to_owned(),
            },
            Snapshot::NotRead => String::new(),
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
        if self.pending {
            match notice {
                Some(Notice::OpEnded {
                    outcome: OpOutcome::Succeeded,
                    ..
                }) => {
                    self.pending = false;
                    self.amend = false;
                    self.draft = None;
                    self.editor.update(cx, |editor, cx| editor.set_text("", cx));
                }
                Some(_) => self.pending = false,
                None => {}
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
        if message.trim().is_empty() {
            return;
        }
        self.pending = true;
        let amend = self.amend;
        self.session.update(cx, |session, cx| {
            session.deliver(Event::NoticeDismissed, cx);
            session.deliver(Event::CommitRequested { message, amend }, cx);
        });
    }

    /// Ticking Amend fills the box with the last commit's message; unticking
    /// brings back what was typed before.
    fn set_amend(&mut self, amend: bool, cx: &mut Context<Self>) {
        if amend == self.amend {
            return;
        }
        self.amend = amend;
        if amend {
            self.draft = Some(self.editor.read(cx).text().to_owned());
            let paths = self.session.read(cx).discovered.paths.clone();
            let editor = self.editor.clone();
            cx.spawn(async move |this, cx| {
                let message = cx
                    .background_executor()
                    .spawn(async move { head_message(&paths) })
                    .await;
                if let Ok(Some(text)) = message {
                    this.update(cx, |this, cx| {
                        if this.amend {
                            editor.update(cx, |editor, cx| editor.set_text(&text, cx));
                        }
                    })
                    .ok();
                }
            })
            .detach();
        } else if let Some(draft) = self.draft.take() {
            self.editor
                .update(cx, |editor, cx| editor.set_text(&draft, cx));
        }
        cx.notify();
    }
}

impl Render for CommitBox {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let text = self.editor.read(cx).text().to_owned();
        let hint = subject_hint(&text);
        let (label, enabled) = commit_button(
            self.staged_count(cx),
            &self.branch_name(cx),
            self.amend,
            text.trim().is_empty(),
        );
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
            .child(self.editor.clone())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(SPACE_2))
                    .child(
                        Checkbox::new(
                            "amend",
                            if self.amend {
                                CheckState::Checked
                            } else {
                                CheckState::Unchecked
                            },
                        )
                        .label("Amend last commit")
                        .on_click(move |_, _, cx| {
                            check.update(cx, |this, cx| {
                                let next = !this.amend;
                                this.set_amend(next, cx)
                            })
                        }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(counter_ink))
                            .child(format!("Subject {} / 72", hint.chars)),
                    ),
            )
            .child(
                Button::new("commit", label)
                    .primary()
                    .full_width()
                    .disabled(!enabled)
                    .on_click(move |_, _, cx| commit.update(cx, |this, cx| this.commit(cx))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_button_names_the_files_and_the_branch() {
        assert_eq!(
            commit_button(1, "main", false, false),
            ("Commit 1 file to main".to_owned(), true)
        );
        assert_eq!(
            commit_button(3, "feat/x", false, false),
            ("Commit 3 files to feat/x".to_owned(), true)
        );
    }

    #[test]
    fn an_empty_message_or_nothing_staged_is_not_ready() {
        assert!(!commit_button(2, "main", false, true).1);
        assert!(!commit_button(0, "main", false, false).1);
    }

    #[test]
    fn an_amend_needs_only_a_message() {
        assert_eq!(
            commit_button(0, "main", true, false),
            ("Amend commit".to_owned(), true)
        );
        assert!(!commit_button(0, "main", true, true).1);
    }
}
