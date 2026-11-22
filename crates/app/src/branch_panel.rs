//! The sidebar of a repo tab: branches, remotes, tags and stashes, with the
//! forms to create, rename and delete a branch. It only sends events; the
//! core checks every request against the fresh snapshot and refuses what
//! cannot run (an existing name, the checked-out branch, a busy repo), and
//! the repo view shows the refusal.

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window,
    div, px,
};
use invoke_core::{BranchName, BranchStart, Event, NameProblem, Oid, RefName, Snapshot};
use tokens::{SPACE_2, SPACE_3, TEXT_SMALL};
use ui::convert::hsla;
use ui::{
    Button, CheckState, Checkbox, EditorEvent, SectionLabel, SideAction, Sidebar, TextEditor,
    TextStyleExt, ThemeExt, sidebar_rows,
};

use crate::session::RepoSession;

/// What the form above the list is for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Form {
    None,
    Create,
    Rename(BranchName),
    Delete {
        name: BranchName,
        force: bool,
    },
    /// Save the local changes as a stash.
    Stash {
        untracked: bool,
    },
    /// Ask before a stash entry is thrown away.
    DropStash {
        id: Oid,
        label: String,
        newest: bool,
    },
}

/// Why a typed name is not a branch name, in words.
pub fn name_problem_text(problem: NameProblem) -> &'static str {
    match problem {
        NameProblem::Empty => "Type a name.",
        NameProblem::Reserved => "HEAD is git's own name for the current commit.",
        NameProblem::LeadingDash => "A name cannot start with a dash.",
        NameProblem::BadSlash => "A name cannot start or end with a slash, or have an empty part.",
        NameProblem::BadDot => "A part of a name cannot start with a dot or end with .lock.",
        NameProblem::BadAt => "A name cannot contain @{ or be just @.",
        NameProblem::BadCharacter => {
            "A name cannot contain spaces or any of ~ ^ : ? * [ and backslash."
        }
    }
}

/// The local name for checking out `origin/feature/x`: everything after the
/// first slash.
pub fn local_name_of(remote_branch: &str) -> &str {
    remote_branch
        .split_once('/')
        .map_or(remote_branch, |(_, name)| name)
}

/// What the drop question says. Only the newest stash can be put back by
/// Undo (a stash below it cannot go back to its place).
pub fn drop_text(label: &str, newest: bool) -> String {
    let what =
        format!("Drop stash {label}? The changes saved in it are removed from the stash list.");
    if newest {
        format!("{what} You can undo this.")
    } else {
        format!("{what} This cannot be undone, because it is not the newest stash.")
    }
}

/// The line under the stash message: what will and will not be saved.
pub fn stash_hint(has_changes: bool, untracked: bool) -> &'static str {
    match (has_changes, untracked) {
        (false, _) => "There are no local changes to stash.",
        (true, true) => "Saves staged, unstaged and new files, then restores a clean tree.",
        (true, false) => "Saves staged and unstaged changes of tracked files; new files stay.",
    }
}

pub struct BranchPanel {
    session: Entity<RepoSession>,
    editor: Entity<TextEditor>,
    /// The stash message.
    message: Entity<TextEditor>,
    form: Form,
    _subscriptions: Vec<Subscription>,
}

impl BranchPanel {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| TextEditor::new("Branch name", 1, cx).single_line());
        let message = cx.new(|cx| TextEditor::new("Message (optional)", 1, cx).single_line());
        let subscriptions = vec![
            cx.subscribe(&editor, |this, _, event: &EditorEvent, cx| match event {
                EditorEvent::Changed => cx.notify(),
                EditorEvent::Submit => this.submit(false, cx),
            }),
            cx.subscribe(&message, |this, _, event: &EditorEvent, cx| match event {
                EditorEvent::Changed => cx.notify(),
                EditorEvent::Submit => this.submit(false, cx),
            }),
            cx.observe(&session, |_, _, cx| cx.notify()),
        ];
        Self {
            session,
            editor,
            message,
            form: Form::None,
            _subscriptions: subscriptions,
        }
    }

    fn deliver(&self, event: Event, cx: &mut Context<Self>) {
        self.session.update(cx, |session, cx| {
            session.deliver(Event::NoticeDismissed, cx);
            session.deliver(event, cx)
        });
    }

    fn open(&mut self, form: Form, text: &str, cx: &mut Context<Self>) {
        self.form = form;
        self.editor
            .update(cx, |editor, cx| editor.set_text(text, cx));
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.form = Form::None;
        cx.notify();
    }

    /// The oid a `refs/...` name points at, in the newest read.
    fn oid_of(&self, name: &str, cx: &Context<Self>) -> Option<Oid> {
        match &self.session.read(cx).state.snapshot {
            Snapshot::Read { disk, .. } => disk.refs.refs.get(&RefName(name.to_owned())).cloned(),
            Snapshot::NotRead => None,
        }
    }

    fn action(&mut self, action: SideAction, cx: &mut Context<Self>) {
        match action {
            SideAction::Switch(name) => {
                self.deliver(Event::BranchSwitchRequested(BranchName(name)), cx)
            }
            SideAction::Rename(name) => {
                let text = name.clone();
                self.open(Form::Rename(BranchName(name)), &text, cx)
            }
            SideAction::Delete(name) => self.open(
                Form::Delete {
                    name: BranchName(name),
                    force: false,
                },
                "",
                cx,
            ),
            SideAction::CheckOutRemote(remote_branch) => {
                let local = local_name_of(&remote_branch).to_owned();
                let exists = self.oid_of(&format!("refs/heads/{local}"), cx).is_some();
                if exists {
                    self.deliver(Event::BranchSwitchRequested(BranchName(local)), cx);
                } else if let Some(at) = self.oid_of(&format!("refs/remotes/{remote_branch}"), cx) {
                    self.deliver(
                        Event::BranchCreateRequested {
                            name: local,
                            at: BranchStart::Commit(at),
                            switch: true,
                        },
                        cx,
                    );
                }
            }
            SideAction::StashApply(index) => self.stash(index, StashOp::Apply, cx),
            SideAction::StashPop(index) => self.stash(index, StashOp::Pop, cx),
            SideAction::StashDrop(index) => self.ask_to_drop(index, cx),
        }
    }

    /// Opens the question before a stash entry is dropped.
    fn ask_to_drop(&mut self, index: usize, cx: &mut Context<Self>) {
        let entry = match &self.session.read(cx).state.snapshot {
            Snapshot::Read { disk, .. } => disk.stashes.get(index).cloned(),
            Snapshot::NotRead => None,
        };
        if let Some(entry) = entry {
            self.open(
                Form::DropStash {
                    id: entry.id,
                    label: entry.message,
                    newest: index == 0,
                },
                "",
                cx,
            );
        }
    }

    /// Opens the form that saves the local changes as a stash.
    fn ask_to_stash(&mut self, cx: &mut Context<Self>) {
        self.message
            .update(cx, |message, cx| message.set_text("", cx));
        self.form = Form::Stash { untracked: false };
        cx.notify();
    }

    fn stash(&mut self, index: usize, op: StashOp, cx: &mut Context<Self>) {
        let id = match &self.session.read(cx).state.snapshot {
            Snapshot::Read { disk, .. } => disk.stashes.get(index).map(|entry| entry.id.clone()),
            Snapshot::NotRead => None,
        };
        let Some(id) = id else { return };
        self.deliver(
            match op {
                StashOp::Apply => Event::StashApplyRequested(id),
                StashOp::Pop => Event::StashPopRequested(id),
            },
            cx,
        );
    }

    /// The form's main button (or Enter in its field).
    fn submit(&mut self, switch: bool, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().trim().to_owned();
        match self.form.clone() {
            Form::None => {}
            Form::Create => {
                if BranchName::parse(&text).is_ok() {
                    self.deliver(
                        Event::BranchCreateRequested {
                            name: text,
                            at: BranchStart::Head,
                            switch,
                        },
                        cx,
                    );
                    self.close(cx);
                }
            }
            Form::Rename(from) => {
                if BranchName::parse(&text).is_ok() {
                    self.deliver(Event::BranchRenameRequested { from, to: text }, cx);
                    self.close(cx);
                }
            }
            Form::Stash { untracked } => {
                let message = self.message.read(cx).text().to_owned();
                self.deliver(
                    Event::StashSaveRequested {
                        message,
                        include_untracked: untracked,
                    },
                    cx,
                );
                self.close(cx);
            }
            Form::DropStash { id, .. } => {
                self.deliver(Event::StashDropRequested(id), cx);
                self.close(cx);
            }
            Form::Delete { name, force } => {
                self.deliver(Event::BranchDeleteRequested { name, force }, cx);
                self.close(cx);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum StashOp {
    Apply,
    Pop,
}

impl Render for BranchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let rows = match &self.session.read(cx).state.snapshot {
            Snapshot::Read { disk, .. } => sidebar_rows(&disk.refs, &disk.head, &disk.stashes),
            Snapshot::NotRead => Vec::new(),
        };
        let this = cx.entity();
        let new = cx.entity();
        let stash = cx.entity();
        let mut panel = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(hsla(p.bg_panel))
            .py(px(SPACE_2))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .px(px(SPACE_3))
                    .pb(px(SPACE_2))
                    .child(SectionLabel::new("Branches"))
                    .child(
                        div()
                            .flex()
                            .gap(px(SPACE_2))
                            .child(Button::new("stash-open", "Stash").ghost().on_click(
                                move |_, _, cx| stash.update(cx, |this, cx| this.ask_to_stash(cx)),
                            ))
                            .child(Button::new("new-branch", "+ Branch").ghost().on_click(
                                move |_, _, cx| {
                                    new.update(cx, |this, cx| this.open(Form::Create, "", cx))
                                },
                            )),
                    ),
            );

        let text = self.editor.read(cx).text().trim().to_owned();
        let problem = BranchName::parse(&text).err();
        let (close, ok, ok_switch) = (cx.entity(), cx.entity(), cx.entity());
        let cancel = Button::new("form-cancel", "Cancel")
            .ghost()
            .on_click(move |_, _, cx| close.update(cx, |this, cx| this.close(cx)));
        let form = match self.form.clone() {
            Form::None => None,
            Form::Create => Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_2))
                    .child(self.editor.clone())
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(if text.is_empty() {
                                p.ink_faint
                            } else {
                                p.danger
                            }))
                            .child(match problem {
                                Some(problem) if !text.is_empty() => name_problem_text(problem),
                                _ => "From the commit HEAD is on.",
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(SPACE_2))
                            .child(
                                Button::new("form-create", "Create")
                                    .disabled(problem.is_some())
                                    .on_click(move |_, _, cx| {
                                        ok.update(cx, |this, cx| this.submit(false, cx))
                                    }),
                            )
                            .child(
                                Button::new("form-create-switch", "Create and switch")
                                    .primary()
                                    .disabled(problem.is_some())
                                    .on_click(move |_, _, cx| {
                                        ok_switch.update(cx, |this, cx| this.submit(true, cx))
                                    }),
                            )
                            .child(cancel),
                    ),
            ),
            Form::Rename(from) => Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_2))
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(format!("Rename {}", from.0)),
                    )
                    .child(self.editor.clone())
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.danger))
                            .child(match problem {
                                Some(problem) if !text.is_empty() => name_problem_text(problem),
                                _ => "",
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(SPACE_2))
                            .child(
                                Button::new("form-rename", "Rename")
                                    .primary()
                                    .disabled(problem.is_some())
                                    .on_click(move |_, _, cx| {
                                        ok.update(cx, |this, cx| this.submit(false, cx))
                                    }),
                            )
                            .child(cancel),
                    ),
            ),
            Form::Stash { untracked } => {
                let has_changes = match &self.session.read(cx).state.snapshot {
                    Snapshot::Read { disk, .. } => {
                        disk.status.unstaged().next().is_some()
                            || disk.status.staged().next().is_some()
                    }
                    Snapshot::NotRead => false,
                };
                let toggle = cx.entity();
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(SPACE_2))
                        .child(self.message.clone())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(SPACE_2))
                                .child(
                                    Checkbox::new(
                                        "stash-untracked",
                                        if untracked {
                                            CheckState::Checked
                                        } else {
                                            CheckState::Unchecked
                                        },
                                    )
                                    .on_click(
                                        move |_, _, cx| {
                                            toggle.update(cx, |this, cx| {
                                                if let Form::Stash { untracked } = &mut this.form {
                                                    *untracked = !*untracked;
                                                }
                                                cx.notify();
                                            })
                                        },
                                    ),
                                )
                                .child(
                                    div()
                                        .text_style(&TEXT_SMALL, cx)
                                        .text_color(hsla(p.ink_muted))
                                        .child("Include untracked files"),
                                ),
                        )
                        .child(
                            div()
                                .text_style(&TEXT_SMALL, cx)
                                .text_color(hsla(p.ink_faint))
                                .child(stash_hint(has_changes, untracked)),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(SPACE_2))
                                .child(
                                    Button::new("form-stash", "Stash changes")
                                        .primary()
                                        .disabled(!has_changes)
                                        .on_click(move |_, _, cx| {
                                            ok.update(cx, |this, cx| this.submit(false, cx))
                                        }),
                                )
                                .child(cancel),
                        ),
                )
            }
            Form::DropStash { label, newest, .. } => Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_2))
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink))
                            .child(drop_text(&label, newest)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(SPACE_2))
                            .child(
                                Button::new("form-drop-stash", "Drop stash")
                                    .danger()
                                    .on_click(move |_, _, cx| {
                                        ok.update(cx, |this, cx| this.submit(false, cx))
                                    }),
                            )
                            .child(cancel),
                    ),
            ),
            Form::Delete { name, force } => {
                let toggle = cx.entity();
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(SPACE_2))
                        .child(
                            div()
                                .text_style(&TEXT_SMALL, cx)
                                .text_color(hsla(p.ink))
                                .child(format!(
                                    "Delete branch {}? Its commits stay in the history if another branch has them. You can undo this.",
                                    name.0
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(SPACE_2))
                                .child(
                                    Checkbox::new(
                                        "form-force",
                                        if force {
                                            CheckState::Checked
                                        } else {
                                            CheckState::Unchecked
                                        },
                                    )
                                    .on_click(move |_, _, cx| {
                                        toggle.update(cx, |this, cx| {
                                            if let Form::Delete { force, .. } = &mut this.form {
                                                *force = !*force;
                                            }
                                            cx.notify();
                                        })
                                    }),
                                )
                                .child(
                                    div()
                                        .text_style(&TEXT_SMALL, cx)
                                        .text_color(hsla(p.ink_muted))
                                        .child("Delete even if it is not merged"),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(SPACE_2))
                                .child(Button::new("form-delete", "Delete branch").danger().on_click(
                                    move |_, _, cx| ok.update(cx, |this, cx| this.submit(false, cx)),
                                ))
                                .child(cancel),
                        ),
                )
            }
        };
        if let Some(form) = form {
            panel = panel.child(
                div()
                    .flex_none()
                    .px(px(SPACE_3))
                    .pb(px(SPACE_3))
                    .child(form),
            );
        }
        panel.child(Sidebar::new(rows, move |action, _, cx| {
            this.update(cx, |this, cx| this.action(action, cx))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_name_drops_the_remote() {
        assert_eq!(local_name_of("origin/main"), "main");
        assert_eq!(local_name_of("origin/feature/x"), "feature/x");
        assert_eq!(local_name_of("lonely"), "lonely");
    }

    #[test]
    fn every_name_problem_has_words() {
        for problem in [
            NameProblem::Empty,
            NameProblem::Reserved,
            NameProblem::LeadingDash,
            NameProblem::BadSlash,
            NameProblem::BadDot,
            NameProblem::BadAt,
            NameProblem::BadCharacter,
        ] {
            assert!(!name_problem_text(problem).is_empty());
        }
    }

    #[test]
    fn the_words_match_what_the_core_refuses() {
        for (text, problem) in [
            ("", NameProblem::Empty),
            ("HEAD", NameProblem::Reserved),
            ("-x", NameProblem::LeadingDash),
            ("a//b", NameProblem::BadSlash),
            ("a..b", NameProblem::BadDot),
            ("a@{b", NameProblem::BadAt),
            ("a b", NameProblem::BadCharacter),
        ] {
            assert_eq!(BranchName::parse(text).err(), Some(problem), "{text:?}");
        }
    }
}
