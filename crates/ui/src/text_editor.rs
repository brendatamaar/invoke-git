//! A multi-line text editor for commit messages (backlog task 14): soft
//! wrapping, selection by keyboard and mouse, double-click to select a word,
//! clipboard, undo, and input-method (IME) composition drawn inline.
//!
//! GPUI has no text input widget at the pinned revision; it provides the
//! platform plumbing (`EntityInputHandler`, marked text for IME) and the
//! text shaping. The text, selection and undo live in the pure
//! [`crate::editor_model::Editor`]; this file is the view: layout, painting,
//! mouse, key actions and the input handler. Adapted from GPUI's
//! `examples/input.rs` (Apache-2.0) via the Phase 0 spike.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, InteractiveElement,
    IntoElement, KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PaintQuad, ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement,
    Style, Styled, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine, actions, div,
    fill, point, px, relative, size,
};

use crate::convert::hsla;
use crate::editor_model::Editor;
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;
use tokens::{RADIUS_MD, SPACE_1, SPACE_2, TEXT_BODY};

actions!(
    text_editor,
    [
        Backspace,
        Delete,
        DeleteWordBackward,
        DeleteWordForward,
        Left,
        Right,
        Up,
        Down,
        WordLeft,
        WordRight,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectWordLeft,
        SelectWordRight,
        Home,
        End,
        SelectHome,
        SelectEnd,
        DocumentStart,
        DocumentEnd,
        SelectAll,
        Paste,
        Cut,
        Copy,
        Undo,
        Redo,
        Newline,
        Submit,
    ]
);

/// Registers the editor's key bindings. Call once at start-up.
pub fn bind_keys(cx: &mut App) {
    let ctx = Some("TextEditor");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("ctrl-backspace", DeleteWordBackward, ctx),
        KeyBinding::new("ctrl-delete", DeleteWordForward, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("ctrl-left", WordLeft, ctx),
        KeyBinding::new("ctrl-right", WordRight, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("ctrl-shift-left", SelectWordLeft, ctx),
        KeyBinding::new("ctrl-shift-right", SelectWordRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("ctrl-home", DocumentStart, ctx),
        KeyBinding::new("ctrl-end", DocumentEnd, ctx),
        KeyBinding::new("ctrl-a", SelectAll, ctx),
        KeyBinding::new("ctrl-v", Paste, ctx),
        KeyBinding::new("ctrl-c", Copy, ctx),
        KeyBinding::new("ctrl-x", Cut, ctx),
        KeyBinding::new("ctrl-z", Undo, ctx),
        KeyBinding::new("ctrl-y", Redo, ctx),
        KeyBinding::new("ctrl-shift-z", Redo, ctx),
        KeyBinding::new("enter", Newline, ctx),
        KeyBinding::new("ctrl-enter", Submit, ctx),
    ]);
}

/// What the editor tells its owner.
pub enum EditorEvent {
    /// The text changed.
    Changed,
    /// Ctrl+Enter: the user wants to commit.
    Submit,
}

/// The wrapped lines of the last paint and where they were drawn, for the
/// mouse, the cursor keys and the input method.
struct Cached {
    /// `(start byte, shaped line)` for every hard line.
    lines: Vec<(usize, WrappedLine)>,
    line_height: Pixels,
    bounds: Bounds<Pixels>,
    text: SharedString,
}

impl Cached {
    /// The top of hard line `index`, relative to the bounds.
    fn top_of(&self, index: usize) -> Pixels {
        self.lines[..index]
            .iter()
            .map(|(_, line)| line.size(self.line_height).height)
            .fold(px(0.), |a, b| a + b)
    }

    /// The hard line `offset` is on.
    fn line_of(&self, offset: usize) -> usize {
        self.lines
            .iter()
            .rposition(|(start, _)| *start <= offset)
            .unwrap_or(0)
    }

    /// Where byte `offset` is, relative to the bounds' origin.
    fn position_of(&self, offset: usize) -> Option<Point<Pixels>> {
        let index = self.line_of(offset);
        let (start, line) = self.lines.get(index)?;
        let local = (offset - start).min(line.len());
        let at = line.position_for_index(local, self.line_height)?;
        Some(point(at.x, at.y + self.top_of(index)))
    }

    /// The byte offset nearest `position` (relative to the bounds).
    fn index_at(&self, position: Point<Pixels>) -> usize {
        let mut top = px(0.);
        for (start, line) in &self.lines {
            let height = line.size(self.line_height).height;
            if position.y < top + height {
                let local = point(position.x, position.y - top);
                let found = line
                    .closest_index_for_position(local, self.line_height)
                    .unwrap_or_else(|nearest| nearest);
                return start + found.min(line.len());
            }
            top += height;
        }
        self.lines
            .last()
            .map_or(0, |(start, line)| start + line.len())
    }
}

pub struct TextEditor {
    editor: Editor,
    focus: FocusHandle,
    placeholder: SharedString,
    cached: Option<Cached>,
    /// The x the cursor keeps while moving up and down.
    preferred_x: Option<Pixels>,
    selecting: bool,
    min_rows: usize,
    /// One line: Enter submits and line breaks are not accepted (branch
    /// names).
    single_line: bool,
    /// A secret: shown as stars, never copied.
    masked: bool,
}

impl EventEmitter<EditorEvent> for TextEditor {}

impl Focusable for TextEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TextEditor {
    pub fn new(
        placeholder: impl Into<SharedString>,
        min_rows: usize,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            editor: Editor::default(),
            focus: cx.focus_handle().tab_stop(true),
            placeholder: placeholder.into(),
            cached: None,
            preferred_x: None,
            selecting: false,
            min_rows: min_rows.max(1),
            single_line: false,
            masked: false,
        }
    }

    /// Makes this a one-line field: Enter submits, pasted line breaks become
    /// spaces.
    pub fn single_line(mut self) -> Self {
        self.single_line = true;
        self
    }

    /// Makes this a secret field (a password): one line, shown as stars,
    /// and Copy and Cut do not put it on the clipboard.
    pub fn masked(mut self) -> Self {
        self.single_line = true;
        self.masked = true;
        self
    }

    pub fn text(&self) -> &str {
        self.editor.text()
    }

    pub fn is_empty(&self) -> bool {
        self.editor.is_empty()
    }

    /// Replaces the text (an amended message, or clearing after a commit).
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.editor.set_text(text);
        self.preferred_x = None;
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preferred_x = None;
        window.notify_text_input_changed(&self.focus);
        cx.emit(EditorEvent::Changed);
        cx.notify();
    }

    fn moved(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    // ---- actions ----------------------------------------------------------

    fn step(&mut self, forward: bool, extend: bool, cx: &mut Context<Self>) {
        self.preferred_x = None;
        let selection = self.editor.selection();
        let target = if !extend && !selection.is_empty() {
            if forward {
                selection.end
            } else {
                selection.start
            }
        } else if forward {
            self.editor.next_char(self.editor.cursor())
        } else {
            self.editor.prev_char(self.editor.cursor())
        };
        self.editor.move_to(target, extend);
        self.moved(cx);
    }

    fn word(&mut self, forward: bool, extend: bool, cx: &mut Context<Self>) {
        self.preferred_x = None;
        let cursor = self.editor.cursor();
        let target = if forward {
            self.editor.word_end_after(cursor)
        } else {
            self.editor.word_start_before(cursor)
        };
        self.editor.move_to(target, extend);
        self.moved(cx);
    }

    fn line_edge(&mut self, end: bool, extend: bool, cx: &mut Context<Self>) {
        self.preferred_x = None;
        let cursor = self.editor.cursor();
        let target = if end {
            self.editor.line_end(cursor)
        } else {
            self.editor.line_start(cursor)
        };
        self.editor.move_to(target, extend);
        self.moved(cx);
    }

    fn vertical(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        let Some(cached) = &self.cached else { return };
        let cursor = self.editor.cursor();
        let Some(here) = cached.position_of(cursor) else {
            return;
        };
        let x = self.preferred_x.unwrap_or(here.x);
        let line_height = cached.line_height;
        let probe = point(
            x,
            if down {
                here.y + line_height * 1.5
            } else {
                here.y - line_height * 0.5
            },
        );
        let target = if probe.y < px(0.) {
            0
        } else {
            let last = cached.index_at(point(px(100_000.), px(100_000.)));
            let found = cached.index_at(probe);
            // Moving down from the last row goes to the end of the text.
            if down && cached.position_of(last).is_some_and(|end| end.y <= here.y) {
                last
            } else {
                found
            }
        };
        self.editor.move_to(target, extend);
        self.preferred_x = Some(x);
        self.moved(cx);
    }

    fn copy_selection(&self, cx: &mut Context<Self>) {
        if !self.masked && !self.editor.selection().is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.editor.selected_text().to_owned(),
            ));
        }
    }

    // ---- mouse ------------------------------------------------------------

    fn index_for_mouse(&self, position: Point<Pixels>) -> usize {
        match &self.cached {
            Some(cached) => cached.index_at(point(
                position.x - cached.bounds.left(),
                position.y - cached.bounds.top(),
            )),
            None => 0,
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window, cx);
        self.preferred_x = None;
        let index = self.index_for_mouse(event.position);
        if event.click_count >= 2 {
            let word = self.editor.word_range_at(index);
            self.editor.select_range(word);
        } else {
            self.selecting = true;
            self.editor.move_to(index, event.modifiers.shift);
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            let index = self.index_for_mouse(event.position);
            self.editor.move_to(index, true);
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }
}

impl EntityInputHandler for TextEditor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.editor.range_from_utf16(&range_utf16);
        actual_range.replace(self.editor.range_to_utf16(&range));
        Some(self.editor.text()[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let selection = self.editor.selection();
        Some(UTF16Selection {
            range: self.editor.range_to_utf16(&selection),
            reversed: self.editor.cursor() == selection.start && !selection.is_empty(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.editor
            .marked()
            .map(|range| self.editor.range_to_utf16(&range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.unmark();
        window.notify_text_input_changed(&self.focus);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16.map(|range| self.editor.range_from_utf16(&range));
        self.editor.replace_text_in_range(range, new_text);
        self.changed(window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16.map(|range| self.editor.range_from_utf16(&range));
        // The selection the platform names is relative to the new text, in
        // UTF-16 units; convert it to bytes of `new_text`.
        let selected = new_selected_range_utf16.map(|selected| {
            let to_byte = |units: usize| {
                let mut count = 0;
                for (index, c) in new_text.char_indices() {
                    if count >= units {
                        return index;
                    }
                    count += c.len_utf16();
                }
                new_text.len()
            };
            to_byte(selected.start)..to_byte(selected.end)
        });
        self.editor.replace_and_mark(range, new_text, selected);
        self.changed(window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let cached = self.cached.as_ref()?;
        if cached.text.as_ref() != self.editor.text() {
            return None;
        }
        let range = self.editor.range_from_utf16(&range_utf16);
        let start = cached.position_of(range.start)?;
        let end = cached.position_of(range.end)?;
        let right = if end.y == start.y {
            end.x.max(start.x + px(1.))
        } else {
            cached.bounds.size.width
        };
        Some(Bounds::from_corners(
            point(bounds.left() + start.x, bounds.top() + start.y),
            point(
                bounds.left() + right,
                bounds.top() + start.y + cached.line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let cached = self.cached.as_ref()?;
        if cached.text.as_ref() != self.editor.text() {
            return None;
        }
        let local = cached.bounds.localize(&position)?;
        Some(self.editor.offset_to_utf16(cached.index_at(local)))
    }
}

/// The stars shown for a secret: as many bytes as the text has, so every
/// offset of the real text is also an offset of the shown text.
pub fn mask_text(text: &str) -> String {
    "*".repeat(text.len())
}

/// Shapes the whole text with `wrap` as the line width. The marked span is
/// underlined (IME composition shows inline, in the text).
fn shape(
    text: &SharedString,
    marked: Option<Range<usize>>,
    style: &gpui::TextStyle,
    window: &mut Window,
    wrap: Pixels,
    color: gpui::Hsla,
) -> Vec<(usize, WrappedLine)> {
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs: Vec<TextRun> = match marked {
        Some(marked) if marked.end <= text.len() => [
            TextRun {
                len: marked.start,
                ..run.clone()
            },
            TextRun {
                len: marked.end - marked.start,
                underline: Some(UnderlineStyle {
                    color: Some(color),
                    thickness: px(1.),
                    wavy: false,
                }),
                ..run.clone()
            },
            TextRun {
                len: text.len() - marked.end,
                ..run
            },
        ]
        .into_iter()
        .filter(|run| run.len > 0)
        .collect(),
        _ => vec![run],
    };
    let font_size = style.font_size.to_pixels(window.rem_size());
    let lines = window
        .text_system()
        .shape_text(text.clone(), font_size, &runs, Some(wrap), None)
        .unwrap_or_default();
    let mut start = 0;
    lines
        .into_iter()
        .map(|line| {
            let at = start;
            start += line.len() + 1;
            (at, line)
        })
        .collect()
}

struct EditorElement {
    input: Entity<TextEditor>,
}

struct Prepaint {
    lines: Vec<(usize, WrappedLine)>,
    selection: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    line_height: Pixels,
    placeholder: bool,
}

impl gpui::Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let input = self.input.clone();
        // Capture the element's text style now: the measure callback runs
        // later during layout after `with_text_style` scopes have popped.
        let text_style = window.text_style();
        let layout = window.request_measured_layout(style, move |known, available, window, cx| {
            let editor = input.read(cx);
            let text: SharedString = editor.editor.text().to_owned().into();
            let rows = editor.min_rows;
            let line_height = text_style.line_height_in_pixels(window.rem_size());
            let width = known.width.unwrap_or(match available.width {
                gpui::AvailableSpace::Definite(width) => width,
                _ => px(300.),
            });
            let color = hsla(cx.palette().ink);
            let shown: SharedString = if text.is_empty() {
                " ".into()
            } else if editor.masked {
                mask_text(&text).into()
            } else {
                text
            };
            let lines = shape(&shown, None, &text_style, window, width, color);
            let height = lines
                .iter()
                .map(|(_, line)| line.size(line_height).height)
                .fold(px(0.), |a, b| a + b);
            size(width, height.max(line_height * rows))
        });
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let p = cx.palette();
        let input = self.input.read(cx);
        let text_style = window.text_style();
        let line_height = text_style.line_height_in_pixels(window.rem_size());
        let empty = input.editor.is_empty();
        let (text, color): (SharedString, _) = if empty {
            (input.placeholder.clone(), hsla(p.ink_faint))
        } else {
            let text = input.editor.text();
            let shown: SharedString = if input.masked {
                mask_text(text).into()
            } else {
                text.to_owned().into()
            };
            (shown, hsla(p.ink))
        };
        let marked = input.editor.marked();
        let selection = input.editor.selection();
        let cursor = input.editor.cursor();
        let lines = shape(&text, marked, &text_style, window, bounds.size.width, color);
        let cached = Cached {
            lines,
            line_height,
            bounds,
            text: text.clone(),
        };
        let mut quads = Vec::new();
        let tint = hsla(p.accent).opacity(0.33);
        if !empty && !selection.is_empty() {
            for (index, (start, line)) in cached.lines.iter().enumerate() {
                let end = start + line.len();
                let (from, to) = (selection.start.max(*start), selection.end.min(end + 1));
                if from >= to {
                    continue;
                }
                let top = cached.top_of(index);
                // Walk the characters, one rectangle for each visual row.
                let slice = &text[from..to.min(end)];
                let mut row: Option<(Pixels, Pixels, Pixels)> = None; // (y, x0, x1)
                let mut offsets: Vec<usize> = slice.char_indices().map(|(i, _)| from + i).collect();
                offsets.push(to.min(end));
                for pair in offsets.windows(2) {
                    let (Some(a), Some(b)) = (
                        line.position_for_index(pair[0] - start, line_height),
                        line.position_for_index(pair[1] - start, line_height),
                    ) else {
                        continue;
                    };
                    let b_x = if b.y == a.y { b.x } else { bounds.size.width };
                    match &mut row {
                        Some((y, _, x1)) if *y == a.y => *x1 = b_x,
                        _ => {
                            if let Some((y, x0, x1)) = row.take() {
                                quads.push(fill(
                                    Bounds::new(
                                        point(bounds.left() + x0, bounds.top() + top + y),
                                        size(x1 - x0, line_height),
                                    ),
                                    tint,
                                ));
                            }
                            row = Some((a.y, a.x, b_x));
                        }
                    }
                }
                if let Some((y, x0, mut x1)) = row.take() {
                    // A selected line break shows as a little extra width.
                    if selection.end > end && selection.start <= end {
                        x1 += px(6.);
                    }
                    quads.push(fill(
                        Bounds::new(
                            point(bounds.left() + x0, bounds.top() + top + y),
                            size(x1 - x0, line_height),
                        ),
                        tint,
                    ));
                }
            }
        }
        let cursor_quad = (selection.is_empty() || empty)
            .then(|| {
                let at = if empty { 0 } else { cursor };
                cached.position_of(at).map(|pos| {
                    let inset = px(SPACE_1 / 2.0);
                    fill(
                        Bounds::new(
                            point(bounds.left() + pos.x, bounds.top() + pos.y + inset),
                            size(px(2.), (line_height - inset * 2.).max(px(1.))),
                        ),
                        hsla(p.accent),
                    )
                })
            })
            .flatten();
        Prepaint {
            lines: cached.lines.into_iter().collect(),
            selection: quads,
            cursor: cursor_quad,
            line_height,
            placeholder: empty,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        for quad in prepaint.selection.drain(..) {
            window.paint_quad(quad);
        }
        let mut top = px(0.);
        for (_, line) in &prepaint.lines {
            let origin = point(bounds.left(), bounds.top() + top);
            line.paint(
                origin,
                prepaint.line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            )
            .ok();
            top += line.size(prepaint.line_height).height;
        }
        if focus.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        let lines = std::mem::take(&mut prepaint.lines);
        let placeholder = prepaint.placeholder;
        let line_height = prepaint.line_height;
        self.input.update(cx, |input, _| {
            let text: SharedString = if placeholder {
                input.placeholder.clone()
            } else {
                input.editor.text().to_owned().into()
            };
            input.cached = Some(Cached {
                lines,
                line_height,
                bounds,
                text,
            });
        });
    }
}

impl Render for TextEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let focused = self.focus.is_focused(window);
        macro_rules! on {
            ($div:expr, $action:ty, |$this:ident, $window:ident, $cx:ident| $body:expr) => {
                $div.on_action(cx.listener(
                    |$this, _: &$action, $window: &mut Window, $cx: &mut Context<Self>| $body,
                ))
            };
        }
        let hover_border = hsla(p.ink_muted);
        let ring = focus::ring_color(cx);
        let editor_id = ElementId::from(("text-editor", cx.entity_id().as_u64() as usize));
        let mut root = div()
            .id(editor_id)
            .role(if self.masked {
                gpui::Role::PasswordInput
            } else if self.single_line {
                gpui::Role::TextInput
            } else {
                gpui::Role::MultilineTextInput
            })
            .aria_label(self.placeholder.clone())
            .aria_placeholder(self.placeholder.clone())
            .key_context("TextEditor")
            .track_focus(&self.focus)
            .tab_stop(true)
            .cursor(CursorStyle::IBeam)
            .w_full()
            .overflow_hidden()
            .px(px(SPACE_2))
            .py(px(if self.single_line { SPACE_1 } else { SPACE_2 }))
            .rounded(px(RADIUS_MD))
            .border_1()
            .border_color(hsla(if focused { p.accent } else { p.line_control }))
            .bg(hsla(p.bg_app))
            .text_style(&TEXT_BODY, cx)
            .focus_visible(move |style| focus::ring(style, ring))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up));
        if !self.masked && window.is_a11y_active() {
            root = root.aria_value(self.text().to_owned());
        }
        if !focused {
            root = root.hover(move |style| style.border_color(hover_border));
        }
        let root = on!(root, Backspace, |this, window, cx| {
            this.editor.backspace();
            this.changed(window, cx)
        });
        let root = on!(root, Delete, |this, window, cx| {
            this.editor.delete_forward();
            this.changed(window, cx)
        });
        let root = on!(root, DeleteWordBackward, |this, window, cx| {
            this.editor.delete_word_backward();
            this.changed(window, cx)
        });
        let root = on!(root, DeleteWordForward, |this, window, cx| {
            this.editor.delete_word_forward();
            this.changed(window, cx)
        });
        let root = on!(root, Left, |this, _w, cx| this.step(false, false, cx));
        let root = on!(root, Right, |this, _w, cx| this.step(true, false, cx));
        let root = on!(root, SelectLeft, |this, _w, cx| this.step(false, true, cx));
        let root = on!(root, SelectRight, |this, _w, cx| this.step(true, true, cx));
        let root = on!(root, Up, |this, _w, cx| this.vertical(false, false, cx));
        let root = on!(root, Down, |this, _w, cx| this.vertical(true, false, cx));
        let root = on!(root, SelectUp, |this, _w, cx| this
            .vertical(false, true, cx));
        let root = on!(root, SelectDown, |this, _w, cx| this
            .vertical(true, true, cx));
        let root = on!(root, WordLeft, |this, _w, cx| this.word(false, false, cx));
        let root = on!(root, WordRight, |this, _w, cx| this.word(true, false, cx));
        let root = on!(root, SelectWordLeft, |this, _w, cx| this
            .word(false, true, cx));
        let root = on!(root, SelectWordRight, |this, _w, cx| this
            .word(true, true, cx));
        let root = on!(root, Home, |this, _w, cx| this.line_edge(false, false, cx));
        let root = on!(root, End, |this, _w, cx| this.line_edge(true, false, cx));
        let root = on!(root, SelectHome, |this, _w, cx| this
            .line_edge(false, true, cx));
        let root = on!(root, SelectEnd, |this, _w, cx| this
            .line_edge(true, true, cx));
        let root = on!(root, DocumentStart, |this, _w, cx| {
            this.editor.move_to(0, false);
            this.moved(cx)
        });
        let root = on!(root, DocumentEnd, |this, _w, cx| {
            let end = this.editor.text().len();
            this.editor.move_to(end, false);
            this.moved(cx)
        });
        let root = on!(root, SelectAll, |this, _w, cx| {
            this.editor.select_all();
            this.moved(cx)
        });
        let root = on!(root, Copy, |this, _w, cx| this.copy_selection(cx));
        let root = on!(root, Cut, |this, window, cx| {
            this.copy_selection(cx);
            if !this.editor.selection().is_empty() {
                this.editor.backspace();
                this.changed(window, cx);
            }
        });
        let root = on!(root, Paste, |this, window, cx| {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                // A pasted line break is a line break, whatever its kind.
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                let text = if this.single_line {
                    text.replace('\n', " ")
                } else {
                    text
                };
                this.editor.insert(&text);
                this.changed(window, cx);
            }
        });
        let root = on!(root, Undo, |this, window, cx| {
            if this.editor.undo() {
                this.changed(window, cx);
            }
        });
        let root = on!(root, Redo, |this, window, cx| {
            if this.editor.redo() {
                this.changed(window, cx);
            }
        });
        let root = on!(root, Newline, |this, window, cx| {
            if this.single_line {
                cx.emit(EditorEvent::Submit);
            } else {
                this.editor.insert("\n");
                this.changed(window, cx)
            }
        });
        let root = on!(root, Submit, |_this, _w, cx| cx.emit(EditorEvent::Submit));
        root.child(EditorElement { input: cx.entity() })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn a_secret_shows_only_stars() {
        assert_eq!(mask_text(""), "");
        assert_eq!(mask_text("hunter2"), "*******");
        assert!(!mask_text("pässwörd").contains('p'));
    }

    proptest! {
        /// Every offset of the real text is an offset of the shown text,
        /// and nothing of the secret shows.
        #[test]
        fn the_mask_keeps_every_offset_and_hides_every_character(secret in any::<String>()) {
            let shown = mask_text(&secret);
            prop_assert_eq!(shown.len(), secret.len());
            prop_assert!(shown.chars().all(|c| c == '*'));
            for (offset, _) in secret.char_indices() {
                prop_assert!(shown.is_char_boundary(offset));
            }
        }
    }
}
