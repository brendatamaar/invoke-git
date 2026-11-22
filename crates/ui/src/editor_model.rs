//! The text and selection of the message editor, with its edits and undo.
//! Pure: no window, no layout. Offsets are byte offsets that always sit on
//! `char` boundaries; the view converts to UTF-16 for the platform's input
//! method (IME). Moving by `char` and not by grapheme keeps the crate
//! dependency free: a base letter with a combining mark takes two steps.

use std::ops::Range;

/// How an edit relates to the one before, for undo grouping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// Typing a run of ordinary characters: one undo step for the run.
    Type,
    /// Deleting backwards or forwards: one step for the run.
    Delete,
    /// Anything else (paste, cut, newline, IME commit): its own step.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    text: String,
    selection: Range<usize>,
    reversed: bool,
}

const UNDO_LIMIT: usize = 200;

#[derive(Clone, Debug, Default)]
pub struct Editor {
    text: String,
    selection: Range<usize>,
    /// The cursor is at the start of the selection (selected backwards).
    reversed: bool,
    /// The span an input method is composing; not yet part of the undo
    /// history.
    marked: Option<Range<usize>>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last: Option<Kind>,
}

impl Editor {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let end = text.len();
        Self {
            text,
            selection: end..end,
            ..Self::default()
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn selection(&self) -> Range<usize> {
        self.selection.clone()
    }

    pub fn cursor(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }

    pub fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    pub fn selected_text(&self) -> &str {
        &self.text[self.selection.clone()]
    }

    /// Replaces everything (loading an amended message, clearing after a
    /// commit). Forgets the history, because the old text is not coming back.
    pub fn set_text(&mut self, text: impl Into<String>) {
        *self = Self::new(text);
    }

    // ---- boundaries -------------------------------------------------------

    fn clamp(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    pub fn prev_char(&self, offset: usize) -> usize {
        self.text[..self.clamp(offset)]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    pub fn next_char(&self, offset: usize) -> usize {
        let offset = self.clamp(offset);
        self.text[offset..]
            .chars()
            .next()
            .map_or(self.text.len(), |c| offset + c.len_utf8())
    }

    fn class(c: char) -> u8 {
        if c.is_alphanumeric() || c == '_' {
            1
        } else if c.is_whitespace() {
            0
        } else {
            2
        }
    }

    /// The start of the word before `offset`: skips spaces, then one run of
    /// letters or one run of punctuation.
    pub fn word_start_before(&self, offset: usize) -> usize {
        let mut at = self.clamp(offset);
        let before = |at: usize| self.text[..at].chars().next_back();
        while let Some(c) = before(at) {
            if c == '\n' || Self::class(c) != 0 {
                break;
            }
            at -= c.len_utf8();
        }
        if let Some(first) = before(at).filter(|c| *c != '\n') {
            let class = Self::class(first);
            while let Some(c) = before(at) {
                if c == '\n' || Self::class(c) != class {
                    break;
                }
                at -= c.len_utf8();
            }
        } else if let Some('\n') = before(at) {
            // At a line start: step over the line break itself.
            at -= 1;
        }
        at
    }

    /// The end of the word after `offset`.
    pub fn word_end_after(&self, offset: usize) -> usize {
        let mut at = self.clamp(offset);
        let after = |at: usize| self.text[at..].chars().next();
        while let Some(c) = after(at) {
            if c == '\n' || Self::class(c) != 0 {
                break;
            }
            at += c.len_utf8();
        }
        if let Some(first) = after(at).filter(|c| *c != '\n') {
            let class = Self::class(first);
            while let Some(c) = after(at) {
                if c == '\n' || Self::class(c) != class {
                    break;
                }
                at += c.len_utf8();
            }
        } else if let Some('\n') = after(at) {
            at += 1;
        }
        at
    }

    /// The word (a run of one class) around `offset`, for double-click.
    pub fn word_range_at(&self, offset: usize) -> Range<usize> {
        let at = self.clamp(offset);
        let class_at = self.text[at..]
            .chars()
            .next()
            .or_else(|| self.text[..at].chars().next_back())
            .map(Self::class);
        let Some(class) = class_at else {
            return at..at;
        };
        let (mut start, mut end) = (at, at);
        while let Some(c) = self.text[..start].chars().next_back() {
            if c == '\n' || Self::class(c) != class {
                break;
            }
            start -= c.len_utf8();
        }
        while let Some(c) = self.text[end..].chars().next() {
            if c == '\n' || Self::class(c) != class {
                break;
            }
            end += c.len_utf8();
        }
        start..end
    }

    /// The start of the line (up to a `\n`) that `offset` is on.
    pub fn line_start(&self, offset: usize) -> usize {
        let at = self.clamp(offset);
        self.text[..at].rfind('\n').map_or(0, |i| i + 1)
    }

    pub fn line_end(&self, offset: usize) -> usize {
        let at = self.clamp(offset);
        self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |i| at + i)
    }

    // ---- selection --------------------------------------------------------

    /// Moves the cursor to `offset`; with `extend` the other end stays put.
    pub fn move_to(&mut self, offset: usize, extend: bool) {
        let offset = self.clamp(offset);
        self.last = None;
        if extend {
            let anchor = if self.reversed {
                self.selection.end
            } else {
                self.selection.start
            };
            self.reversed = offset < anchor;
            self.selection = offset.min(anchor)..offset.max(anchor);
        } else {
            self.reversed = false;
            self.selection = offset..offset;
        }
    }

    pub fn select_all(&mut self) {
        self.last = None;
        self.reversed = false;
        self.selection = 0..self.text.len();
    }

    pub fn select_range(&mut self, range: Range<usize>) {
        self.last = None;
        self.reversed = false;
        self.selection = self.clamp(range.start)..self.clamp(range.end);
    }

    // ---- edits ------------------------------------------------------------

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            selection: self.selection.clone(),
            reversed: self.reversed,
        }
    }

    /// Starts an undo step unless this edit continues the previous one.
    fn record(&mut self, kind: Kind) {
        if kind == Kind::Other || self.last != Some(kind) {
            self.undo.push(self.snapshot());
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last = Some(kind);
    }

    /// Replaces `range` with `text` and puts the cursor after it.
    fn replace(&mut self, range: Range<usize>, text: &str, kind: Kind) {
        let range = self.clamp(range.start)..self.clamp(range.end);
        self.record(kind);
        self.text.replace_range(range.clone(), text);
        let end = range.start + text.len();
        self.selection = end..end;
        self.reversed = false;
        self.marked = None;
    }

    /// Types `text` over the selection. Text with a line break, or a space
    /// after a word, ends the typing run so undo steps stay a word long.
    pub fn insert(&mut self, text: &str) {
        let kind = if text.chars().count() == 1 && !text.contains(char::is_whitespace) {
            Kind::Type
        } else {
            Kind::Other
        };
        self.replace(self.selection.clone(), text, kind);
    }

    pub fn backspace(&mut self) {
        if self.selection.is_empty() {
            let start = self.prev_char(self.selection.start);
            if start == self.selection.start {
                return;
            }
            self.replace(start..self.selection.start, "", Kind::Delete);
        } else {
            self.replace(self.selection.clone(), "", Kind::Other);
        }
    }

    pub fn delete_forward(&mut self) {
        if self.selection.is_empty() {
            let end = self.next_char(self.selection.end);
            if end == self.selection.end {
                return;
            }
            self.replace(self.selection.end..end, "", Kind::Delete);
        } else {
            self.replace(self.selection.clone(), "", Kind::Other);
        }
    }

    pub fn delete_word_backward(&mut self) {
        if self.selection.is_empty() {
            let start = self.word_start_before(self.selection.start);
            if start == self.selection.start {
                return;
            }
            self.replace(start..self.selection.start, "", Kind::Other);
        } else {
            self.replace(self.selection.clone(), "", Kind::Other);
        }
    }

    pub fn delete_word_forward(&mut self) {
        if self.selection.is_empty() {
            let end = self.word_end_after(self.selection.end);
            if end == self.selection.end {
                return;
            }
            self.replace(self.selection.end..end, "", Kind::Other);
        } else {
            self.replace(self.selection.clone(), "", Kind::Other);
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        self.redo.push(self.snapshot());
        self.restore(previous);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(self.snapshot());
        self.restore(next);
        true
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.text = snapshot.text;
        self.selection = snapshot.selection;
        self.reversed = snapshot.reversed;
        self.marked = None;
        self.last = None;
    }

    // ---- input method -----------------------------------------------------

    /// The platform replaces a range with committed text (or the selection
    /// when `range` is `None`; the marked span if one is open).
    pub fn replace_text_in_range(&mut self, range: Option<Range<usize>>, text: &str) {
        let range = range
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone());
        self.replace(range, text, Kind::Other);
    }

    /// The platform sets composition text: the span is marked, the
    /// selection is `selected` relative to the new text.
    pub fn replace_and_mark(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
    ) {
        let range = range
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone());
        let range = self.clamp(range.start)..self.clamp(range.end);
        self.record(Kind::Other);
        self.text.replace_range(range.clone(), text);
        let start = range.start;
        self.marked = (!text.is_empty()).then(|| start..start + text.len());
        let inner = selected.unwrap_or(text.len()..text.len());
        let (a, b) = (
            start + inner.start.min(text.len()),
            start + inner.end.min(text.len()),
        );
        let (a, b) = (self.clamp(a), self.clamp(b));
        self.selection = a.min(b)..a.max(b);
        self.reversed = false;
    }

    pub fn unmark(&mut self) {
        self.marked = None;
    }

    // ---- UTF-16 (the platform input method counts in UTF-16 units) --------

    pub fn offset_to_utf16(&self, offset: usize) -> usize {
        let offset = self.clamp(offset);
        self.text[..offset].chars().map(char::len_utf16).sum()
    }

    pub fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut units = 0;
        for (index, c) in self.text.char_indices() {
            if units >= offset {
                return index;
            }
            units += c.len_utf16();
        }
        self.text.len()
    }

    pub fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn typing_inserts_at_the_cursor_and_moves_it() {
        let mut editor = Editor::default();
        for c in "fix: handle CRLF".chars() {
            editor.insert(&c.to_string());
        }
        assert_eq!(editor.text(), "fix: handle CRLF");
        assert_eq!(editor.cursor(), editor.text().len());
    }

    #[test]
    fn backspace_removes_one_char_or_the_selection() {
        let mut editor = Editor::new("héllo");
        editor.move_to(3, false); // after é
        editor.backspace();
        assert_eq!(editor.text(), "hllo");
        editor.select_range(0..2);
        editor.backspace();
        assert_eq!(editor.text(), "lo");
    }

    #[test]
    fn words_move_over_spaces_then_a_word() {
        let editor = Editor::new("one two  three");
        assert_eq!(editor.word_start_before(14), 9);
        assert_eq!(editor.word_start_before(9), 4);
        assert_eq!(editor.word_end_after(0), 3);
        assert_eq!(editor.word_end_after(3), 7);
    }

    #[test]
    fn double_click_selects_the_word_under_the_pointer() {
        let editor = Editor::new("fix: handle CRLF endings");
        assert_eq!(&editor.text()[editor.word_range_at(7)], "handle");
        // CJK text is one run of letters.
        let cjk = Editor::new("日本語 text");
        assert_eq!(&cjk.text()[cjk.word_range_at(1)], "日本語");
    }

    #[test]
    fn lines_are_found_by_line_breaks() {
        let editor = Editor::new("ab\ncde\nf");
        assert_eq!(editor.line_start(5), 3);
        assert_eq!(editor.line_end(3), 6);
        assert_eq!(editor.line_start(0), 0);
        assert_eq!(editor.line_end(7), 8);
    }

    #[test]
    fn undo_steps_back_by_word_and_redo_returns() {
        let mut editor = Editor::default();
        for c in "ab".chars() {
            editor.insert(&c.to_string());
        }
        editor.insert(" ");
        for c in "cd".chars() {
            editor.insert(&c.to_string());
        }
        assert_eq!(editor.text(), "ab cd");
        assert!(editor.undo());
        assert_eq!(editor.text(), "ab ");
        assert!(editor.undo());
        assert_eq!(editor.text(), "ab");
        assert!(editor.redo());
        assert_eq!(editor.text(), "ab ");
    }

    #[test]
    fn an_edit_after_undo_clears_redo() {
        let mut editor = Editor::new("a");
        editor.insert("b");
        editor.undo();
        editor.insert("c");
        assert!(!editor.redo());
    }

    #[test]
    fn composition_marks_a_span_and_commits_it() {
        let mut editor = Editor::new("x");
        editor.replace_and_mark(None, "にほん", None);
        assert_eq!(editor.marked(), Some(1..10));
        editor.replace_and_mark(None, "日本", None);
        assert_eq!(editor.text(), "x日本");
        editor.replace_text_in_range(None, "日本語");
        assert_eq!(editor.text(), "x日本語");
        assert_eq!(editor.marked(), None);
    }

    #[test]
    fn utf16_offsets_count_surrogate_pairs() {
        let editor = Editor::new("a😀b");
        assert_eq!(editor.offset_to_utf16(5), 3); // after the emoji: 1 + 2 units
        assert_eq!(editor.offset_from_utf16(3), 5);
        assert_eq!(editor.offset_from_utf16(99), editor.text().len());
    }

    #[derive(Clone, Debug)]
    enum Op {
        Insert(String),
        Backspace,
        Delete,
        DeleteWord,
        Move(usize, bool),
        SelectAll,
        Undo,
        Redo,
        Mark(String),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            "[a-z é日\n😀]{0,4}".prop_map(Op::Insert),
            Just(Op::Backspace),
            Just(Op::Delete),
            Just(Op::DeleteWord),
            (0usize..40, any::<bool>()).prop_map(|(o, e)| Op::Move(o, e)),
            Just(Op::SelectAll),
            Just(Op::Undo),
            Just(Op::Redo),
            "[a-zにほ]{0,3}".prop_map(Op::Mark),
        ]
    }

    fn apply(editor: &mut Editor, op: &Op) {
        match op {
            Op::Insert(text) => editor.insert(text),
            Op::Backspace => editor.backspace(),
            Op::Delete => editor.delete_forward(),
            Op::DeleteWord => editor.delete_word_backward(),
            Op::Move(offset, extend) => editor.move_to(*offset, *extend),
            Op::SelectAll => editor.select_all(),
            Op::Undo => {
                editor.undo();
            }
            Op::Redo => {
                editor.redo();
            }
            Op::Mark(text) => editor.replace_and_mark(None, text, None),
        }
    }

    proptest! {
        /// Whatever happens, the selection and the marked span are inside
        /// the text and on char boundaries, and the cursor is an end of the
        /// selection.
        #[test]
        fn the_state_stays_valid(ops in proptest::collection::vec(op(), 0..40)) {
            let mut editor = Editor::default();
            for op in &ops {
                apply(&mut editor, op);
                let text = editor.text();
                let selection = editor.selection();
                prop_assert!(selection.start <= selection.end && selection.end <= text.len());
                prop_assert!(text.is_char_boundary(selection.start));
                prop_assert!(text.is_char_boundary(selection.end));
                let cursor = editor.cursor();
                prop_assert!(cursor == selection.start || cursor == selection.end);
                if let Some(marked) = editor.marked() {
                    prop_assert!(marked.end <= text.len());
                    prop_assert!(text.is_char_boundary(marked.start) && text.is_char_boundary(marked.end));
                }
            }
        }

        /// Undo then redo gives back the same text.
        #[test]
        fn undo_and_redo_are_inverses(ops in proptest::collection::vec(op(), 1..30)) {
            let mut editor = Editor::default();
            for op in &ops {
                if matches!(op, Op::Undo | Op::Redo) { continue; }
                apply(&mut editor, op);
            }
            let after = editor.text().to_owned();
            if editor.undo() {
                prop_assert!(editor.redo());
                prop_assert_eq!(editor.text(), after.as_str());
            }
        }

        /// Moving never changes the text, and deleting a selection removes
        /// exactly it.
        #[test]
        fn moves_keep_the_text_and_delete_removes_the_selection(
            text in "[a-z é日\n]{0,20}",
            a in 0usize..30,
            b in 0usize..30,
        ) {
            let mut editor = Editor::new(text.clone());
            editor.move_to(a, false);
            editor.move_to(b, true);
            prop_assert_eq!(editor.text(), text.as_str());
            let selection = editor.selection();
            let mut expected = text.clone();
            expected.replace_range(selection.clone(), "");
            if !selection.is_empty() {
                editor.backspace();
                prop_assert_eq!(editor.text(), expected.as_str());
                prop_assert_eq!(editor.cursor(), selection.start);
            }
        }

        /// UTF-16 conversion round-trips on char boundaries.
        #[test]
        fn utf16_round_trips(text in "[a-z é日😀]{0,20}") {
            let editor = Editor::new(text.clone());
            for (offset, _) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
                let units = editor.offset_to_utf16(offset);
                prop_assert_eq!(editor.offset_from_utf16(units), offset);
            }
        }

        /// A word boundary search never leaves the text and always makes
        /// progress unless it is at the edge.
        #[test]
        fn word_boundaries_make_progress(text in "[a-z .,\n]{0,24}", at in 0usize..30) {
            let editor = Editor::new(text.clone());
            let at = editor.clamp(at);
            let start = editor.word_start_before(at);
            let end = editor.word_end_after(at);
            prop_assert!(start <= at && at <= end && end <= text.len());
            if at > 0 { prop_assert!(start < at); }
            if at < text.len() { prop_assert!(end > at); }
        }
    }
}
