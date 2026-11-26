//! The in-app credential prompt: what git (or ssh) asked, and a field for
//! the answer. A password or passphrase is masked and never copied. It only
//! reports what the user chose; the owner sends the answer on (a clone job
//! today, fetch, pull and push in task 17).

use gpui::{
    AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, Styled,
    Subscription, Window, div, px,
};
use invoke_core::{AuthAnswer, AuthKind, AuthPrompt};
use tokens::{
    RADIUS_MD, SPACE_2, SPACE_3, SPACE_4, TEXT_BODY, TEXT_BODY_MEDIUM, TEXT_CODE_SMALL, TEXT_SMALL,
};
use ui::convert::hsla;
use ui::{Button, EditorEvent, TextEditor, TextStyleExt, ThemeExt};

/// What the user chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthEvent {
    Answer(AuthAnswer),
    /// Give up: the whole operation is cancelled (there is no "empty
    /// answer"; git would only ask again).
    Cancel,
}

/// The heading for a kind of prompt.
pub fn title(kind: AuthKind) -> &'static str {
    match kind {
        AuthKind::Username => "Git needs a user name",
        AuthKind::Password => "Git needs a password",
        AuthKind::Passphrase => "Git needs the passphrase of your key",
        AuthKind::HostKey => "Git does not know this server yet",
    }
}

/// A line under the heading that says what happens to the answer.
pub fn reassurance(kind: AuthKind) -> &'static str {
    match kind {
        AuthKind::Username | AuthKind::Password | AuthKind::Passphrase => {
            "It goes to git for this operation only. Invoke Git does not store it."
        }
        AuthKind::HostKey => {
            "Trust it only if the fingerprint is the one your host publishes. Git remembers the answer in its own known_hosts file."
        }
    }
}

/// The answer a host-key button gives.
pub fn host_key_answer(trust: bool) -> AuthAnswer {
    AuthAnswer(if trust { "yes" } else { "no" }.to_owned())
}

pub struct AuthPromptView {
    prompt: AuthPrompt,
    field: Option<Entity<TextEditor>>,
    _subscription: Option<Subscription>,
}

impl EventEmitter<AuthEvent> for AuthPromptView {}

impl AuthPromptView {
    pub fn new(prompt: AuthPrompt, cx: &mut Context<Self>) -> Self {
        let field = match prompt.kind {
            AuthKind::HostKey => None,
            AuthKind::Username => {
                Some(cx.new(|cx| TextEditor::new("User name", 1, cx).single_line()))
            }
            AuthKind::Password => Some(cx.new(|cx| TextEditor::new("Password", 1, cx).masked())),
            AuthKind::Passphrase => {
                Some(cx.new(|cx| TextEditor::new("Passphrase", 1, cx).masked()))
            }
        };
        let subscription = field.as_ref().map(|field| {
            cx.subscribe(field, |this, _, event: &EditorEvent, cx| match event {
                EditorEvent::Changed => cx.notify(),
                EditorEvent::Submit => this.submit(cx),
            })
        });
        Self {
            prompt,
            field,
            _subscription: subscription,
        }
    }

    /// Puts the cursor in the field.
    pub fn focus(&self, window: &mut Window, cx: &mut gpui::App) {
        if let Some(field) = &self.field {
            field.update(cx, |field, cx| field.focus(window, cx));
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if let Some(field) = &self.field {
            let answer = AuthAnswer(field.read(cx).text().to_owned());
            cx.emit(AuthEvent::Answer(answer));
        }
    }
}

impl Render for AuthPromptView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let kind = self.prompt.kind;
        let (ok, cancel, trust, refuse) = (cx.entity(), cx.entity(), cx.entity(), cx.entity());
        let buttons =
            match &self.field {
                Some(_) => div()
                    .flex()
                    .gap(px(SPACE_2))
                    .child(
                        Button::new("auth-continue", "Continue")
                            .primary()
                            .on_click(move |_, _, cx| ok.update(cx, |this, cx| this.submit(cx))),
                    )
                    .child(Button::new("auth-cancel", "Cancel").ghost().on_click(
                        move |_, _, cx| cancel.update(cx, |_, cx| cx.emit(AuthEvent::Cancel)),
                    )),
                None => div()
                    .flex()
                    .gap(px(SPACE_2))
                    .child(
                        Button::new("auth-trust", "Trust and continue")
                            .primary()
                            .on_click(move |_, _, cx| {
                                trust.update(cx, |_, cx| {
                                    cx.emit(AuthEvent::Answer(host_key_answer(true)))
                                })
                            }),
                    )
                    .child(Button::new("auth-refuse", "Cancel").ghost().on_click(
                        move |_, _, cx| {
                            refuse.update(cx, |_, cx| {
                                cx.emit(AuthEvent::Answer(host_key_answer(false)))
                            })
                        },
                    )),
            };
        let prompt_body = if kind == AuthKind::HostKey {
            div()
                .p(px(SPACE_3))
                .rounded(px(RADIUS_MD))
                .bg(hsla(p.bg_app))
                .border_1()
                .border_color(hsla(p.line))
                .text_style(&TEXT_CODE_SMALL, cx)
                .text_color(hsla(p.ink))
                .child(self.prompt.text.trim().to_owned())
        } else {
            div()
                .text_style(&TEXT_BODY, cx)
                .text_color(hsla(p.ink_muted))
                .child(self.prompt.text.trim().to_owned())
        };
        div()
            .flex()
            .flex_col()
            .gap(px(SPACE_3))
            .p(px(SPACE_4))
            .rounded(px(RADIUS_MD))
            .bg(hsla(p.bg_raised))
            .border_1()
            .border_color(hsla(p.line_control))
            .child(
                div()
                    .text_style(&TEXT_BODY_MEDIUM, cx)
                    .text_color(hsla(p.ink))
                    .child(title(kind)),
            )
            .child(prompt_body)
            .children(self.field.clone())
            .child(
                div()
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_faint))
                    .child(reassurance(kind)),
            )
            .child(buttons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_heading_and_a_line_about_the_answer() {
        for kind in [
            AuthKind::Username,
            AuthKind::Password,
            AuthKind::Passphrase,
            AuthKind::HostKey,
        ] {
            assert!(!title(kind).is_empty());
            assert!(!reassurance(kind).is_empty());
        }
        assert!(reassurance(AuthKind::Password).contains("does not store it"));
        assert!(reassurance(AuthKind::HostKey).contains("fingerprint"));
    }

    #[test]
    fn a_host_key_answer_is_the_word_ssh_waits_for() {
        assert_eq!(host_key_answer(true), AuthAnswer("yes".into()));
        assert_eq!(host_key_answer(false), AuthAnswer("no".into()));
    }
}
