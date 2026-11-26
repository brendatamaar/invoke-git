//! The conflict screen (task 23): the banner with Continue, Skip and Abort,
//! the files of the paused operation, and the resolver for the chosen file:
//! the current and incoming side of one conflict block, buttons to accept
//! one side or both, and the result, which is the file itself and can be
//! edited before it is marked resolved.
//!
//! The screen is shown because `DiskState.phase` says an operation is paused
//! (rule 4), never because of a notice. Everything that changes the repo is
//! a request to the core: `ResolveFileRequested`, `ContinueRequested`,
//! `SkipRequested`, `AbortRequested`. The one thing done here is writing the
//! edited text to the working-tree file before `Manual` stages it.

use diff::markers::{Block, Take, accept, find_blocks, has_blocks};
use gpui::{
    AppContext, Context, ElementId, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};
use invoke_core::{
    ConflictKind, Event, FileState, Head, Operation, RepoPath, ResolveChoice, Snapshot,
    StageContent,
};
use shell::{read_worktree_file, write_worktree_file};
use tokens::{
    DIALOG, LANE, RADIUS_MD, RADIUS_SM, ROW_LIST, SIDEBAR, SPACE_1, SPACE_2, SPACE_3, SPACE_4,
    TEXT_BODY, TEXT_BODY_MEDIUM, TEXT_CODE_SMALL, TEXT_LABEL, TEXT_SMALL,
};
use ui::convert::hsla;
use ui::focus;
use ui::{Button, EditorEvent, ResizeEdge, ResizeHandle, TextEditor, TextStyleExt, ThemeExt};

use crate::conflict::{self, Eol, Mode, Why};
use crate::session::RepoSession;

/// Width bounds of the conflicted-files aside.
const ASIDE_W: f32 = SIDEBAR + SPACE_4 * 4.0;
const ASIDE_MIN: f32 = SIDEBAR - SPACE_4;
const ASIDE_MAX: f32 = DIALOG - SPACE_4 * 2.0;

/// The paused operation, its files and the checked-out branch's name.
type Paused = (Operation, Vec<(String, FileState)>, String);

/// A file open in the resolver.
struct Open {
    path: String,
    kind: ConflictKind,
    mode: Mode,
}

enum Load {
    Nothing,
    Reading(String),
    Ready(Open),
    Failed(String, String),
}

/// What the file list was last reconciled with: a change here (another
/// rebase step, a file becoming resolved) reloads the resolver; a change
/// that is only a new read does not, so typing is never thrown away.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    operation: Option<Operation>,
    path: Option<String>,
    state: Option<FileState>,
}

pub struct ConflictPane {
    session: Entity<RepoSession>,
    editor: Entity<TextEditor>,
    selected: Option<String>,
    load: Load,
    /// The block the Current and Incoming panels show.
    block: usize,
    confirm_abort: bool,
    /// Something this screen has to say (a file that could not be written).
    problem: Option<String>,
    key: Option<Key>,
    /// Numbers each file read; an answer for an older one is dropped.
    request: u64,
    aside_w: f32,
    _observe: Subscription,
    _edits: Subscription,
}

impl ConflictPane {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| TextEditor::new("The file is empty", 8, cx));
        let edits = cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Changed) {
                cx.notify();
            }
        });
        let observe = cx.observe(&session, |this, _, cx| this.session_changed(cx));
        let mut pane = Self {
            session,
            editor,
            selected: None,
            load: Load::Nothing,
            block: 0,
            confirm_abort: false,
            problem: None,
            key: None,
            request: 0,
            aside_w: ASIDE_W,
            _observe: observe,
            _edits: edits,
        };
        pane.session_changed(cx);
        pane
    }

    fn paused(&self, cx: &Context<Self>) -> Option<Paused> {
        let state = &self.session.read(cx).state;
        let Snapshot::Read { disk, .. } = &state.snapshot else {
            return None;
        };
        let operation = disk.phase.operation()?;
        let files = disk
            .phase
            .conflicts()?
            .files()
            .iter()
            .map(|file| (file.path.0.clone(), file.state))
            .collect();
        let head = match &disk.head {
            Head::Branch { name, .. } => name.0.clone(),
            _ => "your branch".to_owned(),
        };
        Some((operation, files, head))
    }

    /// The file list moved: keep the choice valid and the resolver current.
    fn session_changed(&mut self, cx: &mut Context<Self>) {
        let Some((operation, files, _)) = self.paused(cx) else {
            self.selected = None;
            self.load = Load::Nothing;
            self.key = None;
            self.confirm_abort = false;
            cx.notify();
            return;
        };
        // A file that was just resolved hands the screen to the next one
        // that still needs work.
        let just_resolved = self.key.as_ref().is_some_and(|key| {
            matches!(key.state, Some(FileState::Unresolved(_)))
                && files.iter().any(|(path, state)| {
                    Some(path) == key.path.as_ref() && matches!(state, FileState::Resolved)
                })
        });
        let next_open = files
            .iter()
            .find(|(_, state)| matches!(state, FileState::Unresolved(_)))
            .map(|(path, _)| path.clone());
        let chosen = self
            .selected
            .clone()
            .filter(|_| !(just_resolved && next_open.is_some()))
            .filter(|path| files.iter().any(|(p, _)| p == path))
            .or_else(|| {
                files
                    .iter()
                    .find(|(_, state)| matches!(state, FileState::Unresolved(_)))
                    .or_else(|| files.first())
                    .map(|(path, _)| path.clone())
            });
        let state = chosen
            .as_ref()
            .and_then(|path| files.iter().find(|(p, _)| p == path))
            .map(|(_, state)| *state);
        let key = Key {
            operation: Some(operation),
            path: chosen.clone(),
            state,
        };
        if self.key.as_ref() != Some(&key) {
            self.key = Some(key);
            self.selected = chosen.clone();
            self.problem = None;
            match (chosen, state) {
                (Some(path), Some(FileState::Unresolved(kind))) => self.read_file(path, kind, cx),
                _ => self.load = Load::Nothing,
            }
        }
        cx.notify();
    }

    fn select(&mut self, path: String, cx: &mut Context<Self>) {
        self.selected = Some(path);
        self.key = None;
        self.session_changed(cx);
    }

    /// Reads the file as git left it and decides how it is resolved.
    fn read_file(&mut self, path: String, kind: ConflictKind, cx: &mut Context<Self>) {
        self.request += 1;
        let request = self.request;
        self.block = 0;
        self.load = Load::Reading(path.clone());
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let repo_path = RepoPath(path.clone());
            let read = cx
                .background_executor()
                .spawn(async move { read_worktree_file(&paths, &repo_path) })
                .await;
            this.update(cx, |this, cx| {
                if this.request != request {
                    return;
                }
                match read {
                    Ok(content) => {
                        let mode = conflict::mode_for(kind, &content);
                        if let (Mode::Editor(_), StageContent::Text(text)) = (&mode, &content) {
                            let editable = conflict::to_editor(text);
                            this.editor
                                .update(cx, |editor, cx| editor.set_text(&editable, cx));
                        }
                        this.load = Load::Ready(Open { path, kind, mode });
                    }
                    Err(error) => this.load = Load::Failed(path, error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn deliver(&mut self, event: Event, cx: &mut Context<Self>) {
        self.problem = None;
        self.session
            .update(cx, |session, cx| session.deliver(event, cx));
    }

    fn blocks(&self, cx: &Context<Self>) -> Vec<Block> {
        find_blocks(self.editor.read(cx).text())
    }

    fn take(&mut self, take: Take, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_owned();
        let blocks = find_blocks(&text);
        let Some(block) = blocks.get(self.block.min(blocks.len().saturating_sub(1))) else {
            return;
        };
        let next = accept(&text, block, take);
        self.editor
            .update(cx, |editor, cx| editor.set_text(&next, cx));
        cx.notify();
    }

    fn step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = self.blocks(cx).len();
        if count == 0 {
            return;
        }
        self.block = if forward {
            (self.block + 1) % count
        } else {
            (self.block + count - 1) % count
        };
        cx.notify();
    }

    /// Writes the edited text to the file and asks the core to stage it.
    fn mark_resolved(&mut self, eol: Eol, cx: &mut Context<Self>) {
        let Load::Ready(open) = &self.load else {
            return;
        };
        let text = self.editor.read(cx).text().to_owned();
        if has_blocks(&text) {
            self.problem = Some(
                "The result still has conflict markers. Accept a side for every block first."
                    .into(),
            );
            cx.notify();
            return;
        }
        let path = RepoPath(open.path.clone());
        let paths = self.session.read(cx).discovered.paths.clone();
        let body = conflict::from_editor(&text, eol);
        cx.spawn(async move |this, cx| {
            let target = path.clone();
            let written = cx
                .background_executor()
                .spawn(async move { write_worktree_file(&paths, &target, &body) })
                .await;
            this.update(cx, |this, cx| match written {
                Ok(()) => this.deliver(
                    Event::ResolveFileRequested {
                        path,
                        choice: ResolveChoice::Manual,
                    },
                    cx,
                ),
                Err(error) => {
                    this.problem = Some(format!("Could not save the file: {error}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Marks the file resolved as it is on disk now (for a file edited
    /// elsewhere), unless it still has conflict markers.
    fn mark_as_is(&mut self, cx: &mut Context<Self>) {
        let Load::Ready(open) = &self.load else {
            return;
        };
        let path = RepoPath(open.path.clone());
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let target = path.clone();
            let read = cx
                .background_executor()
                .spawn(async move { read_worktree_file(&paths, &target) })
                .await;
            this.update(cx, |this, cx| match read {
                Ok(StageContent::Text(text)) if has_blocks(&text) => {
                    this.problem =
                        Some("The file still has conflict markers on disk. Fix it first.".into());
                    cx.notify();
                }
                Ok(_) => this.deliver(
                    Event::ResolveFileRequested {
                        path,
                        choice: ResolveChoice::Manual,
                    },
                    cx,
                ),
                Err(error) => {
                    this.problem = Some(format!("Could not read the file: {error}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn resolve_with(&mut self, choice: ResolveChoice, cx: &mut Context<Self>) {
        if let Load::Ready(open) = &self.load {
            let event = Event::ResolveFileRequested {
                path: RepoPath(open.path.clone()),
                choice,
            };
            self.deliver(event, cx);
        }
    }
}

fn code_lines(
    text: &str,
    first: u32,
    cx: &Context<ConflictPane>,
    p: &tokens::Palette,
) -> gpui::Div {
    let mut column = div()
        .flex()
        .flex_col()
        .py(px(SPACE_2))
        .text_style(&TEXT_CODE_SMALL, cx);
    if text.is_empty() {
        return column.child(
            div()
                .px(px(SPACE_3))
                .text_color(hsla(p.ink_muted))
                .child("(nothing)"),
        );
    }
    for (offset, line) in text.lines().enumerate() {
        column = column.child(
            div()
                .flex()
                .child(
                    div()
                        .flex_none()
                        .w(px(56.))
                        .pr(px(SPACE_3))
                        .text_right()
                        .whitespace_nowrap()
                        .text_color(hsla(p.ink_faint))
                        .child((first + offset as u32).to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(hsla(p.ink))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .child(line.trim_end_matches('\r').to_owned()),
                ),
        );
    }
    column
}

impl Render for ConflictPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let Some((operation, files, head)) = self.paused(cx) else {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_style(&TEXT_BODY, cx)
                .text_color(hsla(p.ink_muted))
                .child("Nothing is waiting to be resolved.")
                .into_any_element();
        };
        let session = self.session.read(cx);
        let (conflicts, gate) = match &session.state.snapshot {
            Snapshot::Read { disk, .. } => (
                disk.phase.conflicts().cloned(),
                invoke_core::continue_gate(&disk.phase),
            ),
            Snapshot::NotRead => (None, invoke_core::ContinueGate::NotInProgress),
        };
        let busy = session.state.ops.running.is_some();
        let is_rebase = matches!(operation, Operation::Rebase { .. });
        let (current_name, incoming_name) = conflict::side_names(operation, &head);

        // The banner.
        let progress = conflicts
            .as_ref()
            .map(|c| conflict::progress(c, operation, &head))
            .unwrap_or_default();
        let mut actions = div().flex().flex_wrap().items_center().gap(px(SPACE_2));
        if self.confirm_abort {
            let (yes, no) = (cx.entity(), cx.entity());
            actions = actions
                .child(
                    div()
                        .max_w(px(420.))
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink))
                        .child(conflict::abort_question(operation, &head)),
                )
                .child(
                    Button::new("conflict-abort-yes", conflict::abort_label(operation))
                        .danger()
                        .disabled(busy)
                        .on_click(move |_, _, cx| {
                            yes.update(cx, |this, cx| {
                                this.confirm_abort = false;
                                this.deliver(Event::AbortRequested, cx)
                            })
                        }),
                )
                .child(
                    Button::new("conflict-abort-no", "Keep resolving")
                        .ghost()
                        .on_click(move |_, _, cx| {
                            no.update(cx, |this, cx| {
                                this.confirm_abort = false;
                                cx.notify()
                            })
                        }),
                );
        } else {
            let (abort, skip, cont) = (cx.entity(), cx.entity(), cx.entity());
            if is_rebase {
                actions = actions.child(
                    Button::new("conflict-skip", "Skip this commit")
                        .disabled(busy)
                        .on_click(move |_, _, cx| {
                            skip.update(cx, |this, cx| this.deliver(Event::SkipRequested, cx))
                        }),
                );
            }
            let ready = matches!(gate, invoke_core::ContinueGate::Ready);
            actions = actions
                .child(
                    Button::new("conflict-abort", conflict::abort_label(operation)).on_click(
                        move |_, _, cx| {
                            abort.update(cx, |this, cx| {
                                this.confirm_abort = true;
                                cx.notify()
                            })
                        },
                    ),
                )
                .child(
                    Button::new("conflict-continue", conflict::continue_label(operation))
                        .primary()
                        .disabled(!ready || busy)
                        .on_click(move |_, _, cx| {
                            cont.update(cx, |this, cx| this.deliver(Event::ContinueRequested, cx))
                        }),
                );
        }
        let banner = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(SPACE_3))
            .px(px(SPACE_4))
            .py(px(SPACE_2))
            .bg(hsla(p.warning_soft))
            .border_b_1()
            .border_color(hsla(p.line))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .child(
                        div()
                            .text_style(&TEXT_BODY_MEDIUM, cx)
                            .text_color(hsla(p.warning))
                            .child(conflict::title(operation, &head)),
                    )
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink))
                            .child(progress),
                    ),
            )
            .child(actions);

        // The files.
        let mut list = div().flex().flex_col().gap(px(SPACE_1)).child(
            div()
                .px(px(SPACE_2))
                .text_style(&TEXT_LABEL, cx)
                .text_color(hsla(p.ink_muted))
                .child(format!("FILES · {}", files.len())),
        );
        for (index, (path, state)) in files.iter().enumerate() {
            let chosen = self.selected.as_deref() == Some(path.as_str());
            let (mark, mark_colour, note) = match state {
                FileState::Unresolved(kind) => ("!", p.warning, conflict::kind_text(*kind)),
                FileState::Resolved => ("✓", p.add_ink, "Resolved"),
            };
            let (this, hover, target) = (cx.entity(), hsla(p.bg_hover), path.clone());
            let row_id = ElementId::from(("conflict-file", index));
            let handle = focus::handle(&row_id, window, cx);
            let mut row = div()
                .id(row_id)
                .track_focus(&handle)
                .tab_stop(true)
                .relative()
                .flex()
                .items_center()
                .gap(px(SPACE_2))
                .h(px(ROW_LIST))
                .px(px(SPACE_2))
                .rounded(px(RADIUS_MD))
                .cursor_pointer()
                .focus_visible(move |style| focus::ring(style, ring))
                .on_click(move |_, _, cx| {
                    let target = target.clone();
                    this.update(cx, |pane, cx| pane.select(target, cx))
                })
                .child(
                    div()
                        .flex_none()
                        .w(px(LANE))
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(mark_colour))
                        .child(mark),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_style(&TEXT_BODY, cx)
                        .text_color(hsla(if matches!(state, FileState::Resolved) {
                            p.ink_muted
                        } else {
                            p.ink
                        }))
                        .child(path.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(mark_colour))
                        .child(note),
                );
            row = if chosen {
                row.bg(hsla(p.bg_raised)).child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .top(px(SPACE_1))
                        .bottom(px(SPACE_1))
                        .w(px(focus::ACCENT_BAR_W))
                        .rounded(px(RADIUS_SM))
                        .bg(hsla(p.accent)),
                )
            } else {
                row.hover(move |style| style.bg(hover))
            };
            list = list.child(row);
        }
        let aside = div()
            .id("conflict-aside")
            .flex_none()
            .w(px(self.aside_w))
            .p(px(SPACE_2))
            .overflow_y_scroll()
            .bg(hsla(p.bg_panel))
            .child(list);

        // The resolver.
        let mut main = div()
            .id("conflict-main")
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .gap(px(SPACE_4))
            .p(px(SPACE_4))
            .overflow_y_scroll()
            .bg(hsla(p.bg_panel));
        let note_line = |text: String, cx: &mut Context<Self>| {
            div()
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink_muted))
                .child(text)
        };
        match &self.load {
            Load::Nothing => {
                let text = match self
                    .selected
                    .as_ref()
                    .and_then(|path| files.iter().find(|(p, _)| p == path))
                {
                    Some((path, FileState::Resolved)) => format!(
                        "{path} is resolved. Choose another file, or continue the {} when every file is.",
                        conflict::operation_name(operation)
                    ),
                    _ => "No file to resolve.".to_owned(),
                };
                main = main.child(note_line(text, cx));
            }
            Load::Reading(path) => {
                main = main.child(note_line(format!("Reading {path}…"), cx));
            }
            Load::Failed(path, error) => {
                main = main.child(note_line(format!("Could not read {path}: {error}"), cx));
            }
            Load::Ready(open) => {
                main = main.child(
                    div()
                        .text_style(&TEXT_BODY_MEDIUM, cx)
                        .text_color(hsla(p.ink))
                        .child(open.path.clone()),
                );
                match &open.mode {
                    Mode::Sides(why) => {
                        let (ours, theirs) = conflict::side_buttons(open.kind);
                        let (a, b) = (cx.entity(), cx.entity());
                        let mut buttons = div().flex().gap(px(SPACE_2)).child(
                            Button::new("conflict-ours", ours).disabled(busy).on_click(
                                move |_, _, cx| {
                                    a.update(cx, |this, cx| {
                                        this.resolve_with(ResolveChoice::Ours, cx)
                                    })
                                },
                            ),
                        );
                        if open.kind != ConflictKind::BothDeleted {
                            buttons = buttons.child(
                                Button::new("conflict-theirs", theirs)
                                    .disabled(busy)
                                    .on_click(move |_, _, cx| {
                                        b.update(cx, |this, cx| {
                                            this.resolve_with(ResolveChoice::Theirs, cx)
                                        })
                                    }),
                            );
                        }
                        if matches!(why, Why::TooLarge | Why::MixedEndings) {
                            let c = cx.entity();
                            buttons = buttons.child(
                                Button::new("conflict-as-is", "Mark resolved as it is")
                                    .disabled(busy)
                                    .on_click(move |_, _, cx| {
                                        c.update(cx, |this, cx| this.mark_as_is(cx))
                                    }),
                            );
                        }
                        main = main
                            .child(
                                div()
                                    .text_style(&TEXT_BODY, cx)
                                    .text_color(hsla(p.ink_muted))
                                    .child(conflict::why_text(why, &current_name, &incoming_name)),
                            )
                            .child(buttons);
                    }
                    Mode::Editor(eol) => {
                        let eol = *eol;
                        let blocks = self.blocks(cx);
                        let shown = self.block.min(blocks.len().saturating_sub(1));
                        let editor_text = self.editor.read(cx).text().to_owned();
                        let has_left = has_blocks(&editor_text);
                        let (cur, inc, both, prev, next, resolve) = (
                            cx.entity(),
                            cx.entity(),
                            cx.entity(),
                            cx.entity(),
                            cx.entity(),
                            cx.entity(),
                        );
                        let mut head_row = div().flex().flex_wrap().items_center().gap(px(SPACE_2));
                        match blocks.get(shown) {
                            Some(block) => {
                                head_row = head_row
                                    .child(note_line(
                                        format!(
                                            "Conflict {} of {} · lines {}–{}",
                                            shown + 1,
                                            blocks.len(),
                                            block.first_line,
                                            block.last_line
                                        ),
                                        cx,
                                    ))
                                    .child(div().flex_1())
                                    .child(
                                        Button::new("conflict-prev", "Previous").ghost().on_click(
                                            move |_, _, cx| {
                                                prev.update(cx, |t, cx| t.step(false, cx))
                                            },
                                        ),
                                    )
                                    .child(Button::new("conflict-next", "Next").ghost().on_click(
                                        move |_, _, cx| next.update(cx, |t, cx| t.step(true, cx)),
                                    ))
                                    .child(
                                        Button::new("conflict-current", "Accept current").on_click(
                                            move |_, _, cx| {
                                                cur.update(cx, |t, cx| t.take(Take::Current, cx))
                                            },
                                        ),
                                    )
                                    .child(
                                        Button::new("conflict-incoming", "Accept incoming")
                                            .on_click(move |_, _, cx| {
                                                inc.update(cx, |t, cx| t.take(Take::Incoming, cx))
                                            }),
                                    )
                                    .child(Button::new("conflict-both", "Accept both").on_click(
                                        move |_, _, cx| {
                                            both.update(cx, |t, cx| t.take(Take::Both, cx))
                                        },
                                    ));
                            }
                            None => {
                                head_row = head_row.child(note_line(
                                    "No conflict markers are left in the result.".to_owned(),
                                    cx,
                                ));
                            }
                        }
                        main = main.child(head_row);
                        if let Some(block) = blocks.get(shown) {
                            let cur_first = block.first_line + 1;
                            let cur_lines = block.current.lines().count() as u32;
                            let base_lines = block
                                .base
                                .as_ref()
                                .map_or(0, |b| b.lines().count() as u32 + 1);
                            let inc_first = cur_first + cur_lines + base_lines + 1;
                            let panel =
                                |title: &str,
                                 name: &str,
                                 text: &str,
                                 first: u32,
                                 cx: &mut Context<Self>| {
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .rounded(px(RADIUS_MD))
                                        .border_1()
                                        .border_color(hsla(p.line))
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .px(px(SPACE_3))
                                                .py(px(SPACE_2))
                                                .bg(hsla(p.bg_raised))
                                                .text_style(&TEXT_SMALL, cx)
                                                .child(
                                                    div()
                                                        .text_color(hsla(p.ink))
                                                        .child(format!("{title} · {name}")),
                                                )
                                                .child(div().text_color(hsla(p.ink_muted)).child(
                                                    conflict::lines_text(text.lines().count()),
                                                )),
                                        )
                                        .child(code_lines(text, first, cx, p))
                                };
                            let (cur_label, inc_label) = (
                                if block.current_label.is_empty() || block.current_label == "HEAD" {
                                    current_name.clone()
                                } else {
                                    block.current_label.clone()
                                },
                                if block.incoming_label.is_empty() {
                                    incoming_name.clone()
                                } else {
                                    block.incoming_label.clone()
                                },
                            );
                            main = main.child(
                                div()
                                    .flex()
                                    .gap(px(SPACE_4))
                                    .child(panel(
                                        "Current",
                                        &cur_label,
                                        &block.current,
                                        cur_first,
                                        cx,
                                    ))
                                    .child(panel(
                                        "Incoming",
                                        &inc_label,
                                        &block.incoming,
                                        inc_first,
                                        cx,
                                    )),
                            );
                        }
                        main = main.child(
                            div()
                                .flex()
                                .flex_col()
                                .rounded(px(RADIUS_MD))
                                .border_1()
                                .border_color(hsla(p.line_control))
                                .overflow_hidden()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(SPACE_2))
                                        .px(px(SPACE_3))
                                        .py(px(SPACE_2))
                                        .bg(hsla(p.bg_raised))
                                        .child(
                                            div()
                                                .text_style(&TEXT_BODY_MEDIUM, cx)
                                                .text_color(hsla(p.ink))
                                                .child("Result"),
                                        )
                                        .child(note_line(
                                            format!(
                                                "Editable · this is what gets written to {}",
                                                open.path
                                            ),
                                            cx,
                                        ))
                                        .child(div().flex_1())
                                        .child(
                                            Button::new("conflict-resolved", "Mark file resolved")
                                                .primary()
                                                .disabled(has_left || busy)
                                                .on_click(move |_, _, cx| {
                                                    resolve.update(cx, |t, cx| {
                                                        t.mark_resolved(eol, cx)
                                                    })
                                                }),
                                        ),
                                )
                                .child(div().p(px(SPACE_2)).child(self.editor.clone())),
                        );
                        main = main.child(note_line(
                            "Accept a side, combine both, or edit the result directly. Mark the file resolved when no conflict markers are left."
                                .to_owned(),
                            cx,
                        ));
                    }
                }
            }
        }
        if let Some(problem) = &self.problem {
            main = main.child(
                div()
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.warning))
                    .child(problem.clone()),
            );
        }

        let resize_aside = cx.entity();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(hsla(p.bg_app))
            .child(banner)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(aside)
                    .child(ResizeHandle::new(
                        "conflict-aside-resize",
                        self.aside_w,
                        ASIDE_W,
                        ASIDE_MIN,
                        ASIDE_MAX,
                        ResizeEdge::Right,
                        move |width, _, cx| {
                            resize_aside.update(cx, |pane, cx| {
                                pane.aside_w = width;
                                cx.notify();
                            });
                        },
                    ))
                    .child(main),
            )
            .into_any_element()
    }
}
