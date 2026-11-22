//! The list of changed files next to the diff: a staged section and an
//! unstaged one, a flat list until the status tree (task 11) replaces it.
//! Rows are `row-list` tall, so the list is virtualized.

use std::rc::Rc;

use gpui::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px, uniform_list,
};
use invoke_core::{FileChange, StatusSnapshot};
use tokens::{RADIUS_MD, ROW_LIST, SPACE_2, TEXT_BODY, TEXT_LABEL, TEXT_SMALL};

use crate::convert::hsla;
use crate::focus;
use crate::scrollbar::Scrollbar;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// Which half of the status a file is listed in. A file with both an index
/// change and a worktree change is in both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Staged,
    Unstaged,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Staged => "STAGED",
            Section::Unstaged => "UNSTAGED",
        }
    }
}

/// One row of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListRow {
    Header {
        section: Section,
        count: usize,
    },
    File {
        section: Section,
        path: SharedString,
        change: FileChange,
    },
}

/// The rows for a status: unstaged first, then staged (as in the design),
/// each with a header; an empty section has no rows at all.
pub fn list_rows(status: &StatusSnapshot) -> Vec<ListRow> {
    let mut rows = Vec::new();
    let mut push = |section: Section, files: Vec<(SharedString, FileChange)>| {
        if files.is_empty() {
            return;
        }
        rows.push(ListRow::Header {
            section,
            count: files.len(),
        });
        rows.extend(files.into_iter().map(|(path, change)| ListRow::File {
            section,
            path,
            change,
        }));
    };
    push(
        Section::Unstaged,
        status
            .unstaged()
            .map(|(path, change)| (path.0.clone().into(), change.clone()))
            .collect(),
    );
    push(
        Section::Staged,
        status
            .staged()
            .map(|(path, change)| (path.0.clone().into(), change.clone()))
            .collect(),
    );
    rows
}

/// The letter that names a change, as `git status --short` does (`?` is
/// written `U` here, for untracked).
pub fn letter(change: &FileChange) -> &'static str {
    match change {
        FileChange::Added => "A",
        FileChange::Modified => "M",
        FileChange::Deleted => "D",
        FileChange::Renamed { .. } => "R",
        FileChange::TypeChanged => "T",
        FileChange::Untracked => "U",
    }
}

type OnSelect = Rc<dyn Fn(Section, SharedString, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct ChangeList {
    rows: Rc<[ListRow]>,
    selected: Option<(Section, SharedString)>,
    on_select: OnSelect,
}

impl ChangeList {
    pub fn new(
        rows: Vec<ListRow>,
        selected: Option<(Section, SharedString)>,
        on_select: impl Fn(Section, SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            rows: rows.into(),
            selected,
            on_select: Rc::new(on_select),
        }
    }
}

impl RenderOnce for ChangeList {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let scroll = window
            .use_keyed_state("change-list-scroll", cx, |_, _| {
                gpui::UniformListScrollHandle::new()
            })
            .read(cx)
            .clone();
        let count = self.rows.len();
        let rows = self.rows.clone();
        let (selected, on_select) = (self.selected, self.on_select);
        let list = uniform_list("change-list", count, move |range, window, cx| {
            let p = cx.palette();
            let ring = focus::ring_color(cx);
            range
                .map(|index| match &rows[index] {
                    ListRow::Header { section, count } => div()
                        .flex()
                        .items_center()
                        .h(px(ROW_LIST))
                        .px(px(SPACE_2))
                        .text_style(&TEXT_LABEL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(format!("{} · {count}", section.title()))
                        .into_any_element(),
                    ListRow::File {
                        section,
                        path,
                        change,
                    } => {
                        let chosen = selected
                            .as_ref()
                            .is_some_and(|(s, p)| s == section && p == path);
                        let id = ElementId::from(("change-row", index));
                        let handle = focus::handle(&id, window, cx);
                        let (section, path) = (*section, path.clone());
                        let on_select = on_select.clone();
                        let hover = hsla(p.bg_hover);
                        let colour = match change {
                            FileChange::Modified | FileChange::TypeChanged => p.warning,
                            FileChange::Added | FileChange::Untracked => p.add_ink,
                            FileChange::Deleted => p.remove_ink,
                            FileChange::Renamed { .. } => p.accent,
                        };
                        let row = div()
                            .id(id)
                            .track_focus(&handle)
                            .tab_stop(true)
                            .flex()
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
                                    .w(px(14.))
                                    .text_style(&TEXT_SMALL, cx)
                                    .font_weight(gpui::FontWeight(600.0))
                                    .text_color(hsla(colour))
                                    .child(letter(change)),
                            )
                            .child(div().flex_1().min_w(px(0.)).truncate().child(path.clone()))
                            .on_click(move |_, window, cx| {
                                on_select(section, path.clone(), window, cx)
                            });
                        if chosen {
                            row.bg(hsla(p.bg_raised)).into_any_element()
                        } else {
                            row.hover(move |style| style.bg(hover)).into_any_element()
                        }
                    }
                })
                .collect()
        })
        .track_scroll(&scroll)
        .flex_1()
        .h_full();
        div()
            .flex()
            .flex_1()
            .min_h(px(0.))
            .px(px(SPACE_2))
            .child(list)
            .child(Scrollbar::new(
                "change-list-scrollbar",
                scroll,
                count,
                ROW_LIST,
            ))
    }
}

#[cfg(test)]
mod tests {
    use invoke_core::{RepoPath, StatusEntry};

    use super::*;

    fn entry(path: &str, staged: Option<FileChange>, unstaged: Option<FileChange>) -> StatusEntry {
        StatusEntry {
            path: RepoPath(path.into()),
            staged,
            unstaged,
        }
    }

    #[test]
    fn unstaged_comes_first_and_empty_sections_vanish() {
        let status = StatusSnapshot::from_entries([
            entry("b.rs", Some(FileChange::Added), None),
            entry("a.rs", None, Some(FileChange::Modified)),
        ]);
        let rows = list_rows(&status);
        assert_eq!(rows.len(), 4);
        assert!(matches!(
            rows[0],
            ListRow::Header {
                section: Section::Unstaged,
                count: 1
            }
        ));
        assert!(matches!(
            rows[2],
            ListRow::Header {
                section: Section::Staged,
                count: 1
            }
        ));
        let only_staged =
            StatusSnapshot::from_entries([entry("x", Some(FileChange::Deleted), None)]);
        assert_eq!(list_rows(&only_staged).len(), 2);
        assert!(list_rows(&StatusSnapshot::default()).is_empty());
    }

    #[test]
    fn a_file_changed_in_both_places_is_listed_twice() {
        let status = StatusSnapshot::from_entries([entry(
            "a.rs",
            Some(FileChange::Modified),
            Some(FileChange::Modified),
        )]);
        let files = list_rows(&status)
            .into_iter()
            .filter(|r| matches!(r, ListRow::File { .. }))
            .count();
        assert_eq!(files, 2);
    }

    #[test]
    fn letters_follow_git_status() {
        assert_eq!(letter(&FileChange::Modified), "M");
        assert_eq!(letter(&FileChange::Untracked), "U");
        assert_eq!(letter(&FileChange::Deleted), "D");
    }
}
