//! The status tree (backlog task 11): the changed files as a tree of
//! folders, in a staged and an unstaged section, with a checkbox on every
//! file and folder to stage or unstage it. The rows are a pure function of
//! the status and of which folders are collapsed, so they are tested without
//! a window; the component only draws them.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce, SharedString,
    StatefulInteractiveElement, Styled, UniformListScrollHandle, Window, div, px, uniform_list,
};
use invoke_core::{FileChange, StatusSnapshot};
use tokens::{RADIUS_MD, ROW_LIST, SPACE_2, SPACE_3, TEXT_BODY, TEXT_LABEL, TEXT_SMALL};

use crate::changes::{Section, letter};
use crate::components::{Button, CheckState, Checkbox};
use crate::convert::hsla;
use crate::focus;
use crate::scrollbar::Scrollbar;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// One row of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeRow {
    Header {
        section: Section,
        count: usize,
    },
    Dir {
        section: Section,
        /// The folder's full path, without a trailing slash.
        path: String,
        name: String,
        depth: usize,
        expanded: bool,
        /// Files below it (in this section).
        files: usize,
    },
    File {
        section: Section,
        path: String,
        name: String,
        depth: usize,
        change: FileChange,
    },
}

/// What the user did in the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeAction {
    /// Show the file's diff.
    Select { section: Section, path: String },
    /// Stage (from the unstaged section) or unstage (from the staged one).
    ToggleFile { section: Section, path: String },
    /// The same for every file in a folder.
    ToggleDir { section: Section, dir: String },
    /// Fold or unfold a folder.
    Collapse { section: Section, dir: String },
    /// Stage or unstage the whole section.
    All { section: Section },
}

/// The rows for `status`: unstaged first, then staged; a section with no
/// files has no rows. Folders in `collapsed` hide what is below them.
pub fn tree_rows(status: &StatusSnapshot, collapsed: &HashSet<(Section, String)>) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for section in [Section::Unstaged, Section::Staged] {
        let files: Vec<(&str, &FileChange)> = match section {
            Section::Unstaged => status
                .unstaged()
                .map(|(path, change)| (path.0.as_str(), change))
                .collect(),
            Section::Staged => status
                .staged()
                .map(|(path, change)| (path.0.as_str(), change))
                .collect(),
        };
        if files.is_empty() {
            continue;
        }
        rows.push(TreeRow::Header {
            section,
            count: files.len(),
        });
        // Files under each folder.
        let mut below: HashMap<&str, usize> = HashMap::new();
        for (path, _) in &files {
            let mut end = 0;
            while let Some(at) = path[end..].find('/') {
                end += at;
                *below.entry(&path[..end]).or_default() += 1;
                end += 1;
            }
        }
        // Folders open on the way to the current file, outermost first.
        let mut open: Vec<&str> = Vec::new();
        for (path, change) in files {
            let parts: Vec<&str> = path.split('/').collect();
            let dirs = &parts[..parts.len() - 1];
            // Keep the folders this file shares with the previous one.
            let shared = open
                .iter()
                .zip(dirs)
                .take_while(|(open, part)| open.rsplit('/').next() == Some(**part))
                .count();
            // A folder path matches only if every level matches; `shared`
            // counted names, so confirm with full paths.
            let mut keep = 0;
            let mut prefix_end = 0;
            for (depth, part) in dirs.iter().enumerate().take(shared) {
                prefix_end += part.len() + usize::from(depth > 0);
                if depth < open.len() && open[depth] == &path[..prefix_end] {
                    keep = depth + 1;
                } else {
                    break;
                }
            }
            open.truncate(keep);
            // Open the new folders, unless a parent is folded.
            let mut hidden = open
                .iter()
                .any(|dir| collapsed.contains(&(section, (*dir).to_owned())));
            let mut end = open.last().map_or(0, |dir| dir.len() + 1);
            for (depth, part) in dirs.iter().enumerate().skip(open.len()) {
                let dir_end = end + part.len();
                let dir = &path[..dir_end];
                open.push(dir);
                if !hidden {
                    let folded = collapsed.contains(&(section, dir.to_owned()));
                    rows.push(TreeRow::Dir {
                        section,
                        path: dir.to_owned(),
                        name: (*part).to_owned(),
                        depth,
                        expanded: !folded,
                        files: below[dir],
                    });
                    hidden = folded;
                }
                end = dir_end + 1;
            }
            if !hidden {
                rows.push(TreeRow::File {
                    section,
                    path: path.to_owned(),
                    name: parts[parts.len() - 1].to_owned(),
                    depth: dirs.len(),
                    change: change.clone(),
                });
            }
        }
    }
    rows
}

type OnAction = Rc<dyn Fn(TreeAction, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct StatusTree {
    rows: Rc<[TreeRow]>,
    selected: Option<(Section, SharedString)>,
    on_action: OnAction,
}

impl StatusTree {
    pub fn new(
        rows: Vec<TreeRow>,
        selected: Option<(Section, SharedString)>,
        on_action: impl Fn(TreeAction, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            rows: rows.into(),
            selected,
            on_action: Rc::new(on_action),
        }
    }
}

impl RenderOnce for StatusTree {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let scroll = window
            .use_keyed_state("status-tree-scroll", cx, |_, _| {
                UniformListScrollHandle::new()
            })
            .read(cx)
            .clone();
        let count = self.rows.len();
        let rows = self.rows.clone();
        let (selected, on_action) = (self.selected, self.on_action);
        let list = uniform_list("status-tree", count, move |range, window, cx| {
            let p = cx.palette();
            let ring = focus::ring_color(cx);
            range
                .map(|index| match &rows[index] {
                    TreeRow::Header { section, count } => {
                        let section = *section;
                        let on_action = on_action.clone();
                        let (title, action) = match section {
                            Section::Unstaged => ("UNSTAGED", "Stage all"),
                            Section::Staged => ("STAGED", "Unstage all"),
                        };
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .h(px(ROW_LIST))
                            .px(px(SPACE_2))
                            .child(
                                div()
                                    .text_style(&TEXT_LABEL, cx)
                                    .text_color(hsla(p.ink_muted))
                                    .child(format!("{title} · {count}")),
                            )
                            .child(Button::new(("tree-all", index), action).ghost().on_click(
                                move |_, window, cx| {
                                    on_action(TreeAction::All { section }, window, cx)
                                },
                            ))
                            .into_any_element()
                    }
                    TreeRow::Dir {
                        section,
                        path,
                        name,
                        depth,
                        expanded,
                        files,
                    } => {
                        let (section, path) = (*section, path.clone());
                        let state = match section {
                            Section::Staged => CheckState::Checked,
                            Section::Unstaged => CheckState::Unchecked,
                        };
                        let (toggle, fold) = (on_action.clone(), on_action.clone());
                        let (toggle_path, fold_path) = (path.clone(), path.clone());
                        let id = ElementId::from(("tree-dir", index));
                        let handle = focus::handle(&id, window, cx);
                        div()
                            .id(id)
                            .track_focus(&handle)
                            .tab_stop(true)
                            .flex()
                            .items_center()
                            .gap(px(SPACE_2))
                            .h(px(ROW_LIST))
                            .pl(px(SPACE_2 + *depth as f32 * SPACE_3))
                            .pr(px(SPACE_2))
                            .rounded(px(RADIUS_MD))
                            .text_style(&TEXT_BODY, cx)
                            .text_color(hsla(p.ink_muted))
                            .cursor_pointer()
                            .focus_visible(move |style| focus::ring(style, ring))
                            .on_click(move |_, window, cx| {
                                fold(
                                    TreeAction::Collapse {
                                        section,
                                        dir: fold_path.clone(),
                                    },
                                    window,
                                    cx,
                                )
                            })
                            .child(div().flex_none().w(px(12.)).child(if *expanded {
                                "▾"
                            } else {
                                "▸"
                            }))
                            .child(Checkbox::new(("tree-dir-check", index), state).on_click(
                                move |_, window, cx| {
                                    cx.stop_propagation();
                                    toggle(
                                        TreeAction::ToggleDir {
                                            section,
                                            dir: toggle_path.clone(),
                                        },
                                        window,
                                        cx,
                                    )
                                },
                            ))
                            .child(div().flex_1().min_w(px(0.)).truncate().child(name.clone()))
                            .child(
                                div()
                                    .flex_none()
                                    .text_style(&TEXT_SMALL, cx)
                                    .text_color(hsla(p.ink_faint))
                                    .child(files.to_string()),
                            )
                            .into_any_element()
                    }
                    TreeRow::File {
                        section,
                        path,
                        name,
                        depth,
                        change,
                    } => {
                        let (section, path) = (*section, path.clone());
                        let chosen = selected.as_ref().is_some_and(|(s, selected)| {
                            *s == section && selected.as_ref() == path
                        });
                        let state = match section {
                            Section::Staged => CheckState::Checked,
                            Section::Unstaged => CheckState::Unchecked,
                        };
                        let (select, toggle) = (on_action.clone(), on_action.clone());
                        let (select_path, toggle_path) = (path.clone(), path.clone());
                        let id = ElementId::from(("tree-file", index));
                        let handle = focus::handle(&id, window, cx);
                        let colour = match change {
                            FileChange::Modified | FileChange::TypeChanged => p.warning,
                            FileChange::Added | FileChange::Untracked => p.add_ink,
                            FileChange::Deleted => p.remove_ink,
                            FileChange::Renamed { .. } => p.accent,
                        };
                        let hover = hsla(p.bg_hover);
                        let row = div()
                            .id(id)
                            .track_focus(&handle)
                            .tab_stop(true)
                            .flex()
                            .items_center()
                            .gap(px(SPACE_2))
                            .h(px(ROW_LIST))
                            .pl(px(SPACE_2 + 12.0 + SPACE_2 + *depth as f32 * SPACE_3))
                            .pr(px(SPACE_2))
                            .rounded(px(RADIUS_MD))
                            .text_style(&TEXT_BODY, cx)
                            .text_color(hsla(p.ink))
                            .cursor_pointer()
                            .focus_visible(move |style| focus::ring(style, ring))
                            .on_click(move |_, window, cx| {
                                select(
                                    TreeAction::Select {
                                        section,
                                        path: select_path.clone(),
                                    },
                                    window,
                                    cx,
                                )
                            })
                            .child(Checkbox::new(("tree-file-check", index), state).on_click(
                                move |_, window, cx| {
                                    cx.stop_propagation();
                                    toggle(
                                        TreeAction::ToggleFile {
                                            section,
                                            path: toggle_path.clone(),
                                        },
                                        window,
                                        cx,
                                    )
                                },
                            ))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(14.))
                                    .text_style(&TEXT_SMALL, cx)
                                    .font_weight(gpui::FontWeight(600.0))
                                    .text_color(hsla(colour))
                                    .child(letter(change)),
                            )
                            .child(div().flex_1().min_w(px(0.)).truncate().child(name.clone()));
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
                "status-tree-scrollbar",
                scroll,
                count,
                ROW_LIST,
            ))
    }
}

#[cfg(test)]
mod tests {
    use invoke_core::{RepoPath, StatusEntry};
    use proptest::prelude::*;

    use super::*;

    fn entry(path: &str, staged: bool) -> StatusEntry {
        let change = Some(FileChange::Modified);
        StatusEntry {
            path: RepoPath(path.into()),
            staged: if staged { change.clone() } else { None },
            unstaged: if staged { None } else { change },
        }
    }

    fn status(paths: &[&str], staged: bool) -> StatusSnapshot {
        StatusSnapshot::from_entries(paths.iter().map(|p| entry(p, staged)))
    }

    fn names(rows: &[TreeRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                TreeRow::Header { section, count } => format!("# {section:?} {count}"),
                TreeRow::Dir {
                    name,
                    depth,
                    expanded,
                    files,
                    ..
                } => format!(
                    "{}{name}/ {}{files}",
                    "  ".repeat(*depth),
                    if *expanded { "" } else { "+" }
                ),
                TreeRow::File { name, depth, .. } => format!("{}{name}", "  ".repeat(*depth)),
            })
            .collect()
    }

    #[test]
    fn folders_group_their_files() {
        let rows = tree_rows(
            &status(
                &["a.rs", "src/b.rs", "src/c.rs", "src/deep/d.rs", "z.rs"],
                false,
            ),
            &HashSet::new(),
        );
        assert_eq!(
            names(&rows),
            vec![
                "# Unstaged 5",
                "a.rs",
                "src/ 3",
                "  b.rs",
                "  c.rs",
                "  deep/ 1",
                "    d.rs",
                "z.rs",
            ]
        );
    }

    #[test]
    fn a_folded_folder_hides_what_is_below_it() {
        let mut collapsed = HashSet::new();
        collapsed.insert((Section::Unstaged, "src".to_owned()));
        let rows = tree_rows(
            &status(&["src/b.rs", "src/deep/d.rs", "z.rs"], false),
            &collapsed,
        );
        assert_eq!(names(&rows), vec!["# Unstaged 3", "src/ +2", "z.rs"]);
    }

    #[test]
    fn a_folder_with_the_same_name_in_two_places_is_two_folders() {
        let rows = tree_rows(&status(&["a/x/1.rs", "b/x/2.rs"], false), &HashSet::new());
        let dirs = rows
            .iter()
            .filter(|row| matches!(row, TreeRow::Dir { name, .. } if name == "x"))
            .count();
        assert_eq!(dirs, 2);
    }

    #[test]
    fn staged_and_unstaged_have_their_own_sections() {
        let snapshot = StatusSnapshot::from_entries([entry("a.rs", false), entry("b.rs", true)]);
        let rows = tree_rows(&snapshot, &HashSet::new());
        assert_eq!(
            names(&rows),
            vec!["# Unstaged 1", "a.rs", "# Staged 1", "b.rs"]
        );
        assert!(tree_rows(&StatusSnapshot::default(), &HashSet::new()).is_empty());
    }

    fn path() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![Just("a"), Just("b"), Just("c.rs"), Just("d")],
            1..5,
        )
        .prop_map(|parts| parts.join("/"))
    }

    proptest! {
        /// Unfolded, every file appears once, with a folder row before the
        /// first file below it, and the depth is the number of folders above.
        #[test]
        fn every_file_once_under_its_folders(paths in proptest::collection::btree_set(path(), 1..14)) {
            // A path cannot be both a file and a folder.
            let paths: Vec<String> = paths.into_iter().collect();
            prop_assume!(!paths.iter().any(|p| paths.iter().any(|q| q.starts_with(&format!("{p}/")))));
            let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            let rows = tree_rows(&status(&refs, false), &HashSet::new());
            let files: Vec<&TreeRow> = rows.iter().filter(|r| matches!(r, TreeRow::File { .. })).collect();
            prop_assert_eq!(files.len(), paths.len());
            let mut seen_dirs: HashSet<String> = HashSet::new();
            for row in &rows {
                match row {
                    TreeRow::Dir { path, depth, .. } => {
                        prop_assert_eq!(*depth, path.matches('/').count());
                        seen_dirs.insert(path.clone());
                    }
                    TreeRow::File { path, depth, .. } => {
                        prop_assert_eq!(*depth, path.matches('/').count());
                        // Every ancestor folder was listed before the file.
                        let mut end = 0;
                        while let Some(at) = path[end..].find('/') {
                            end += at;
                            prop_assert!(seen_dirs.contains(&path[..end]), "{} before its folder", path);
                            end += 1;
                        }
                    }
                    TreeRow::Header { .. } => {}
                }
            }
        }

        /// Folding the top folders hides exactly their files.
        #[test]
        fn folding_hides_exactly_the_files_below(
            paths in proptest::collection::btree_set(path(), 1..14),
            fold in proptest::sample::select(vec!["a", "b", "d"]),
        ) {
            let paths: Vec<String> = paths.into_iter().collect();
            prop_assume!(!paths.iter().any(|p| paths.iter().any(|q| q.starts_with(&format!("{p}/")))));
            let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            let mut collapsed = HashSet::new();
            collapsed.insert((Section::Unstaged, fold.to_owned()));
            let rows = tree_rows(&status(&refs, false), &collapsed);
            let shown: HashSet<&str> = rows
                .iter()
                .filter_map(|r| match r { TreeRow::File { path, .. } => Some(path.as_str()), _ => None })
                .collect();
            for p in &paths {
                let hidden = p.starts_with(&format!("{fold}/"));
                prop_assert_eq!(shown.contains(p.as_str()), !hidden, "{}", p);
            }
        }
    }
}
