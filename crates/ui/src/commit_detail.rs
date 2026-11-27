//! The commit detail beside the graph (task 19): the message, who and when,
//! the parents, the files the commit changed and, below them, the diff of
//! the chosen file (the caller passes the diff view as a child).

use std::rc::Rc;

use gpui::{
    AnyElement, App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px, uniform_list,
};
use invoke_core::FileChange;
use tokens::{
    LANE, RADIUS_MD, RADIUS_SM, ROW_LIST, SPACE_1, SPACE_2, SPACE_3, SPACE_4, TEXT_BODY,
    TEXT_CODE_SMALL, TEXT_LABEL, TEXT_SMALL, TEXT_TITLE,
};

use crate::changes::letter;
use crate::components::Button;
use crate::convert::hsla;
use crate::focus;
use crate::scrollbar::Scrollbar;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// How many file rows show before the list scrolls.
const VISIBLE_FILES: usize = 7;

/// The subject (first line) and the body of a commit message: the body
/// starts after the blank lines that follow the subject and loses trailing
/// whitespace. Line endings of either kind are accepted.
pub fn split_message(message: &str) -> (String, String) {
    let message = message.replace("\r\n", "\n").replace('\r', "\n");
    let (subject, rest) = message.split_once('\n').unwrap_or((message.as_str(), ""));
    (
        subject.trim_end().to_owned(),
        rest.trim_start_matches('\n').trim_end().to_owned(),
    )
}

/// A commit id as the design shows it: the first seven characters.
pub fn short_hash(hash: &str) -> &str {
    let end = hash
        .char_indices()
        .nth(7)
        .map_or(hash.len(), |(index, _)| index);
    &hash[..end]
}

/// What a file row shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitFile {
    pub path: SharedString,
    pub change: FileChange,
}

/// Everything the panel prints about a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub hash: SharedString,
    pub subject: SharedString,
    pub body: SharedString,
    pub author: SharedString,
    pub date: SharedString,
    pub parents: Vec<SharedString>,
    pub files: Vec<CommitFile>,
}

impl CommitInfo {
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

type OnSelect = Rc<dyn Fn(usize, &mut Window, &mut App)>;
type OnCopy = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct CommitDetail {
    info: CommitInfo,
    selected: Option<usize>,
    on_select: OnSelect,
    on_copy: OnCopy,
    actions: Vec<AnyElement>,
    diff: Option<AnyElement>,
}

impl CommitDetail {
    pub fn new(
        info: CommitInfo,
        selected: Option<usize>,
        on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
        on_copy: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            info,
            selected,
            on_select: Rc::new(on_select),
            on_copy: Rc::new(on_copy),
            actions: Vec::new(),
            diff: None,
        }
    }

    pub fn actions(mut self, actions: impl IntoIterator<Item = Button>) -> Self {
        self.actions
            .extend(actions.into_iter().map(IntoElement::into_any_element));
        self
    }

    /// The diff of the chosen file, shown below the list.
    pub fn diff(mut self, diff: impl IntoElement) -> Self {
        self.diff = Some(diff.into_any_element());
        self
    }
}

fn change_colour(change: &FileChange, p: &tokens::Palette) -> tokens::Rgba {
    match change {
        FileChange::Modified | FileChange::TypeChanged => p.warning,
        FileChange::Added | FileChange::Untracked => p.add_ink,
        FileChange::Deleted => p.remove_ink,
        FileChange::Renamed { .. } => p.accent,
    }
}

impl RenderOnce for CommitDetail {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let p = cx.palette();
        let info = self.info;
        let (selected, on_select) = (self.selected, self.on_select);
        let on_copy = self.on_copy;
        let count = info.files.len();

        let parents: Vec<String> = info
            .parents
            .iter()
            .map(|parent| short_hash(parent).to_owned())
            .collect();
        let fact = |label: &'static str, value: String, mono: bool, cx: &App| {
            let value_style = if mono { &TEXT_CODE_SMALL } else { &TEXT_SMALL };
            div()
                .flex()
                .gap(px(SPACE_3))
                .child(
                    div()
                        .flex_none()
                        .w(px(72.))
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_style(value_style, cx)
                        .text_color(hsla(p.ink))
                        .child(value),
                )
        };
        let mut facts = div().flex().flex_col().gap(px(SPACE_1));
        facts = facts
            .child(fact("Commit", short_hash(&info.hash).to_owned(), true, cx))
            .child(fact("Author", info.author.to_string(), false, cx))
            .child(fact("Date", info.date.to_string(), false, cx));
        if !parents.is_empty() {
            facts = facts.child(fact(
                if parents.len() > 1 {
                    "Parents"
                } else {
                    "Parent"
                },
                parents.join("  "),
                true,
                cx,
            ));
        }

        let mut header = div().flex().flex_col().flex_none().gap(px(SPACE_2)).child(
            div()
                .text_style(&TEXT_TITLE, cx)
                .text_color(hsla(p.ink))
                .child(info.subject.clone()),
        );
        if !info.body.is_empty() {
            header = header.child(
                div()
                    .id("commit-body")
                    .max_h(px(120.))
                    .overflow_y_scroll()
                    .text_style(&TEXT_BODY, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(info.body.clone()),
            );
        }

        let scroll = window
            .use_keyed_state("commit-files-scroll", cx, |_, _| {
                gpui::UniformListScrollHandle::new()
            })
            .read(cx)
            .clone();
        let files: Rc<[CommitFile]> = info.files.clone().into();
        let rows = files.clone();
        let list = uniform_list("commit-files", count, move |range, window, cx| {
            let p = cx.palette();
            let ring = focus::ring_color(cx);
            range
                .map(|index| {
                    let file = &rows[index];
                    let id = ElementId::from(("commit-file", index));
                    let handle = focus::handle(&id, window, cx);
                    let on_select = on_select.clone();
                    let hover = hsla(p.bg_hover);
                    let row = div()
                        .id(id)
                        .track_focus(&handle)
                        .tab_stop(true)
                        .relative()
                        .flex()
                        .w_full()
                        .items_center()
                        .gap(px(SPACE_2))
                        .h(px(ROW_LIST))
                        .px(px(SPACE_2))
                        .rounded(px(RADIUS_MD))
                        .text_style(&TEXT_BODY, cx)
                        .text_color(hsla(p.ink))
                        .cursor_pointer()
                        .focus_visible(move |style| focus::ring(style, ring))
                        .child(
                            div()
                                .flex_none()
                                .w(px(LANE))
                                .text_style(&TEXT_SMALL, cx)
                                .font_weight(gpui::FontWeight(600.0))
                                .text_color(hsla(change_colour(&file.change, p)))
                                .child(letter(&file.change)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .child(file.path.clone()),
                        )
                        .on_click(move |_, window, cx| on_select(index, window, cx));
                    if selected == Some(index) {
                        row.bg(hsla(p.bg_raised))
                            .child(
                                div()
                                    .absolute()
                                    .left(px(0.))
                                    .top(px(SPACE_1))
                                    .bottom(px(SPACE_1))
                                    .w(px(focus::ACCENT_BAR_W))
                                    .rounded(px(RADIUS_SM))
                                    .bg(hsla(p.accent)),
                            )
                            .into_any_element()
                    } else {
                        row.hover(move |style| style.bg(hover)).into_any_element()
                    }
                })
                .collect()
        })
        .track_scroll(&scroll)
        .flex_1()
        .h_full();

        let list_height = count.clamp(1, VISIBLE_FILES) as f32 * ROW_LIST;
        let files_title = if info.is_merge() {
            format!("FILES CHANGED · {count} · against the first parent")
        } else {
            format!("FILES CHANGED · {count}")
        };
        let files_block = div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(SPACE_1))
            .child(
                div()
                    .text_style(&TEXT_LABEL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(files_title),
            )
            .child(if count == 0 {
                div()
                    .h(px(ROW_LIST))
                    .flex()
                    .items_center()
                    .px(px(SPACE_2))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child("This commit changes no file.")
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .h(px(list_height))
                    .child(list)
                    .child(Scrollbar::new(
                        "commit-files-scrollbar",
                        scroll,
                        count,
                        ROW_LIST,
                    ))
                    .into_any_element()
            });

        let copy_hash = Button::new("copy-hash", "Copy hash")
            .ghost()
            .on_click(move |_, window, cx| on_copy(window, cx));

        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h(px(0.))
            .gap(px(SPACE_3))
            .p(px(SPACE_4))
            .bg(hsla(p.bg_panel))
            .child(header)
            .child(facts)
            .child(files_block)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .flex_none()
                    .gap(px(SPACE_2))
                    .child(copy_hash)
                    .children(self.actions),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .border_1()
                    .border_color(hsla(p.line))
                    .rounded(px(RADIUS_MD))
                    .overflow_hidden()
                    .children(self.diff),
            )
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn the_subject_is_the_first_line_and_the_body_follows_the_blank_lines() {
        assert_eq!(
            split_message("Fix it\n\nBecause.\nMore.\n\n"),
            ("Fix it".to_owned(), "Because.\nMore.".to_owned())
        );
        assert_eq!(
            split_message("Only a subject\n"),
            ("Only a subject".into(), String::new())
        );
        assert_eq!(split_message(""), (String::new(), String::new()));
        // A body that starts right on the next line is kept whole.
        assert_eq!(split_message("S\nbody"), ("S".into(), "body".into()));
        // Windows line endings and trailing spaces.
        assert_eq!(
            split_message("S  \r\n\r\nline one\r\nline two\r\n"),
            ("S".into(), "line one\nline two".into())
        );
    }

    #[test]
    fn a_short_hash_is_seven_characters() {
        assert_eq!(short_hash("0123456789abcdef"), "0123456");
        assert_eq!(short_hash("abc"), "abc");
        assert_eq!(short_hash(""), "");
    }

    proptest! {
        /// Nothing but whitespace is lost, and the subject is one line.
        #[test]
        fn splitting_keeps_every_visible_character(message in "\\PC{0,200}(\n\\PC{0,40}){0,6}") {
            let (subject, body) = split_message(&message);
            prop_assert!(!subject.contains('\n') && !subject.contains('\r'));
            let visible = |text: &str| text.chars().filter(|c| !c.is_whitespace()).collect::<String>();
            prop_assert_eq!(visible(&message), visible(&format!("{subject}{body}")));
        }

        #[test]
        fn a_short_hash_never_splits_a_char(hash in "\\PC{0,20}") {
            let short = short_hash(&hash);
            prop_assert!(short.chars().count() <= 7);
            prop_assert!(hash.starts_with(short));
        }
    }
}
