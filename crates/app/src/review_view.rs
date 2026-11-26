//! The pre-push review screen (task 21): the commits that would leave the
//! machine, the files they change as one combined diff or one commit at a
//! time, "viewed" ticks, and the Push button. It reads through
//! `shell::outgoing` on a background thread, reads again whenever the head or
//! the upstream moves (a fetch, a commit, a push), and sends nothing itself:
//! Push is a `PushRequested` like the header's.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    AppContext, Context, ElementId, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    div, px,
};
use invoke_core::notes::{Note, NoteId, open_todos, relocate};
use invoke_core::{Event, Freshness, LockState, RepoPath, Snapshot};
use shell::InputsError;
use shell::commit_detail::{Change, CommitSides, read_commit, read_commit_sides};
use shell::history::CommitId;
use shell::outgoing::{Outgoing, read_outgoing, read_range_sides};
use shell::precondition::RepoPaths;
use tokens::{
    DIALOG, LANE, RADIUS_MD, RADIUS_SM, ROW_LIST, SIDEBAR, SPACE_1, SPACE_2, SPACE_3, SPACE_4,
    TEXT_BODY, TEXT_CODE_SMALL, TEXT_LABEL, TEXT_SMALL, TEXT_TITLE,
};
use ui::convert::hsla;
use ui::focus;
use ui::time::relative;
use ui::{
    Button, CheckState, Checkbox, DiffContent, DiffView, Pill, PillTone, ResizeEdge, ResizeHandle,
    Segmented, TextEditor, TextStyleExt, ThemeExt, diff_view, short_hash,
};

use crate::notes_db::{self, NotesDb};
use crate::review::{self, Key, Scope, Viewed};
use crate::session::RepoSession;
use crate::sync;

/// Width bounds of the commits / files / notes aside.
const ASIDE_W: f32 = SIDEBAR + SPACE_4 * 6.0;
const ASIDE_MIN: f32 = SIDEBAR;
const ASIDE_MAX: f32 = DIALOG + SPACE_4 * 2.0;

/// Files whose line counts are worked out for the list; a bigger list shows
/// counts for the first ones only.
const COUNTED_FILES: usize = 300;

pub enum ReviewEvent {
    /// Leave the review for the Changes page.
    Back,
    /// Open the Pull / Merge Request creation form for this branch (P9).
    CreatePr,
}

struct Row {
    path: String,
    change: Change,
    /// Added and removed lines; `None` until counted, or when the file has
    /// no text diff.
    stats: Option<(u32, u32)>,
}

enum Phase {
    /// Not on screen.
    Idle,
    Loading,
    Failed(String),
    Ready(Box<Ready>),
}

struct Ready {
    out: Outgoing,
    rows: Vec<Row>,
    /// The file list of a commit is still being read.
    listing: bool,
    selected: Option<usize>,
}

/// Where the two sides of a file come from.
#[derive(Clone)]
enum Source {
    Range {
        base: Option<CommitId>,
        head: CommitId,
    },
    Commit(CommitId),
}

impl Source {
    fn sides(&self, paths: &RepoPaths, path: &RepoPath) -> Result<CommitSides, InputsError> {
        match self {
            Source::Range { base, head } => read_range_sides(paths, base.as_ref(), head, path),
            Source::Commit(id) => read_commit_sides(paths, id, path),
        }
    }
}

fn parse(id: &str) -> Option<CommitId> {
    CommitId::from_hex(id.as_bytes()).ok()
}

fn change_letter(change: Change) -> &'static str {
    match change {
        Change::Added => "A",
        Change::Modified => "M",
        Change::Deleted => "D",
        Change::TypeChanged => "T",
    }
}

/// What a note's buttons do.
#[derive(Clone, Copy)]
enum NoteChange {
    /// Mark a to-do resolved (`true`) or open again (`false`).
    Resolve(bool),
    Delete,
}

pub struct ReviewPane {
    session: Entity<RepoSession>,
    diff: Entity<DiffView>,
    phase: Phase,
    scope: Scope,
    /// The commit shown in [`Scope::PerCommit`] (index into the list).
    commit: usize,
    viewed: Viewed,
    unviewed_only: bool,
    range_changed: bool,
    can_create_request: bool,
    push_request: Option<(invoke_core::OpId, Key)>,
    pushed: Option<Key>,
    /// On screen: only then does a moved head or upstream cause a re-read.
    active: bool,
    loaded: Option<Key>,
    /// Numbers each read of the outgoing list; an older answer is dropped.
    reads: u64,
    /// Numbers each file list and its counts.
    listings: u64,
    /// Numbers each diff load.
    requests: u64,
    db: Arc<NotesDb>,
    /// The notes of this repo that belong to this review.
    notes: Vec<Note>,
    selected_note: Option<NoteId>,
    notes_loading: bool,
    /// Why the notes could not be read or saved.
    notes_error: Option<String>,
    note_line: Entity<TextEditor>,
    note_text: Entity<TextEditor>,
    note_todo: bool,
    /// Why the note in the form was not added.
    note_problem: Option<String>,
    /// The file on show and its new side: what a typed line number is
    /// counted in.
    shown: Option<(String, Arc<str>)>,
    aside_w: f32,
    _observe: Subscription,
}

impl EventEmitter<ReviewEvent> for ReviewPane {}

impl ReviewPane {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&session, |this, _, cx| this.session_changed(cx));
        Self {
            session,
            diff: cx.new(|_| DiffView::new()),
            phase: Phase::Idle,
            scope: Scope::Combined,
            commit: 0,
            viewed: Viewed::default(),
            unviewed_only: false,
            range_changed: false,
            can_create_request: false,
            push_request: None,
            pushed: None,
            active: false,
            loaded: None,
            reads: 0,
            listings: 0,
            requests: 0,
            db: NotesDb::global(),
            notes: Vec::new(),
            selected_note: None,
            notes_loading: false,
            notes_error: None,
            note_line: cx.new(|cx| TextEditor::new("Line", 1, cx).single_line()),
            note_text: cx.new(|cx| TextEditor::new("Write a note on this line", 2, cx)),
            note_todo: false,
            note_problem: None,
            shown: None,
            aside_w: ASIDE_W,
            _observe: observe,
        }
    }

    /// The review was opened: read what would be pushed, now.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.pushed = None;
        self.range_changed = false;
        self.reload(cx);
    }

    pub fn close(&mut self) {
        self.active = false;
        self.phase = Phase::Idle;
        self.reads += 1;
        self.listings += 1;
        self.requests += 1;
    }

    fn session_changed(&mut self, cx: &mut Context<Self>) {
        if self.active {
            let state = &self.session.read(cx).state;
            if let Some(invoke_core::Notice::OpEnded { op, outcome }) = &state.notice
                && self.push_request.as_ref().is_some_and(|(id, _)| id == op)
            {
                let key = self.push_request.take().map(|(_, key)| key);
                self.pushed = match outcome {
                    invoke_core::OpOutcome::Succeeded => key,
                    _ => None,
                };
            }
            if matches!(state.snapshot, Snapshot::Read { .. })
                && review::key_of(state) != self.loaded
            {
                self.reload(cx);
                return;
            }
        }
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let state = &self.session.read(cx).state;
        let key = review::key_of(state);
        if key != self.loaded {
            self.range_changed |= self.loaded.is_some();
            self.viewed.clear();
            self.unviewed_only = false;
        }
        self.loaded = key.clone();
        self.reads += 1;
        self.listings += 1;
        self.requests += 1;
        let read = self.reads;
        self.scope = Scope::Combined;
        self.commit = 0;
        self.shown = None;
        self.selected_note = None;
        self.notes.clear();
        self.notes_error = None;
        self.notes_loading = true;
        self.can_create_request = false;
        let Some(key) = key else {
            self.phase = Phase::Failed("Check out a branch with commits to review a push.".into());
            cx.notify();
            return;
        };
        let upstream = key.target.tracking_ref();
        let remote = key.target.remote.clone();
        self.phase = Phase::Loading;
        self.diff
            .update(cx, |diff, cx| diff.set_content(DiffContent::Loading, cx));
        cx.notify();
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let (result, can_create_request) = cx
                .background_executor()
                .spawn(async move {
                    let result = read_outgoing(&paths, Some(&upstream.0));
                    let can_create_request =
                        shell::read::read_remotes(&paths).is_ok_and(|remotes| {
                            remotes.iter().any(|entry| {
                                entry.name == remote
                                    && invoke_core::forge::parse_remote_repo(&entry.url).is_some()
                            })
                        });
                    (result, can_create_request)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.reads != read {
                    return;
                }
                match result {
                    Ok(out) if key.matches_outgoing(&out) => {
                        this.can_create_request = can_create_request;
                        let rows = rows_of(&out.files);
                        this.phase = Phase::Ready(Box::new(Ready {
                            out,
                            rows,
                            listing: false,
                            selected: None,
                        }));
                        this.files_ready(cx);
                        this.refresh_notes(cx);
                    }
                    Ok(_) => {
                        this.phase = Phase::Failed(
                            "The outgoing range changed while loading. Open the review again."
                                .into(),
                        );
                    }
                    Err(error) => {
                        this.phase =
                            Phase::Failed(format!("Could not read what would be pushed: {error}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Where the sides of the files in the current list come from.
    fn source(&self) -> Option<Source> {
        let Phase::Ready(ready) = &self.phase else {
            return None;
        };
        match self.scope {
            Scope::Combined => Some(Source::Range {
                base: ready.out.base.as_deref().and_then(parse),
                head: parse(&ready.out.head)?,
            }),
            Scope::PerCommit => ready
                .out
                .commits
                .get(self.commit)
                .and_then(|commit| parse(&commit.id))
                .map(Source::Commit),
        }
    }

    /// The commit the current list belongs to; `None` for the combined one.
    fn scope_key(&self) -> Option<String> {
        let Phase::Ready(ready) = &self.phase else {
            return None;
        };
        match self.scope {
            Scope::Combined => None,
            Scope::PerCommit => ready.out.commits.get(self.commit).map(|c| c.id.clone()),
        }
    }

    /// Count the files and select the note's file or the first visible file.
    fn files_ready(&mut self, cx: &mut Context<Self>) {
        self.listings += 1;
        let key = self.scope_key();
        let note = self
            .selected_note
            .and_then(|id| self.notes.iter().find(|note| note.id == id));
        let first = match &mut self.phase {
            Phase::Ready(ready) => {
                ready.listing = false;
                ready.selected = note
                    .and_then(|note| {
                        ready
                            .rows
                            .iter()
                            .position(|row| row.path == note.anchor.path)
                    })
                    .or_else(|| {
                        ready.rows.iter().position(|row| {
                            !self.unviewed_only || !self.viewed.is(key.as_deref(), &row.path)
                        })
                    });
                ready.selected
            }
            _ => return,
        };
        self.load_counts(cx);
        match first {
            Some(index) => self.load_diff(index, cx),
            None => {
                self.requests += 1;
                self.shown = None;
                self.diff.update(cx, |diff, cx| {
                    let message = if self.unviewed_only {
                        "No unviewed files in this diff."
                    } else {
                        "No file to show."
                    };
                    diff.set_content(DiffContent::Message(message.into()), cx)
                });
            }
        }
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope == scope {
            return;
        }
        self.selected_note = None;
        self.scope = scope;
        match scope {
            Scope::Combined => {
                if let Phase::Ready(ready) = &mut self.phase {
                    ready.rows = rows_of(&ready.out.files);
                }
                self.files_ready(cx);
            }
            Scope::PerCommit => self.select_commit(self.commit, None, cx),
        }
        cx.notify();
    }

    /// Shows commit `index` on its own, optionally selecting a note's file.
    fn select_commit(&mut self, index: usize, note: Option<NoteId>, cx: &mut Context<Self>) {
        let Phase::Ready(ready) = &mut self.phase else {
            return;
        };
        let Some(commit) = ready.out.commits.get(index) else {
            return;
        };
        let Some(id) = parse(&commit.id) else { return };
        self.scope = Scope::PerCommit;
        self.commit = index;
        self.selected_note = note;
        ready.rows.clear();
        ready.listing = true;
        ready.selected = None;
        self.listings += 1;
        let listing = self.listings;
        self.requests += 1;
        self.shown = None;
        self.diff
            .update(cx, |diff, cx| diff.set_content(DiffContent::Loading, cx));
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { read_commit(&paths, &id) })
                .await;
            this.update(cx, |this, cx| {
                if this.listings != listing {
                    return;
                }
                let Phase::Ready(ready) = &mut this.phase else {
                    return;
                };
                ready.listing = false;
                match read {
                    Ok(detail) => ready.rows = rows_of(&detail.files),
                    Err(error) => {
                        this.diff.update(cx, |diff, cx| {
                            diff.set_content(
                                DiffContent::Message(
                                    format!("Could not read the commit: {error}").into(),
                                ),
                                cx,
                            )
                        });
                        cx.notify();
                        return;
                    }
                }
                this.files_ready(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn select_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let Phase::Ready(ready) = &mut self.phase else {
            return;
        };
        if index >= ready.rows.len() {
            return;
        }
        ready.selected = Some(index);
        self.selected_note = None;
        self.load_diff(index, cx);
        cx.notify();
    }

    fn toggle_viewed(&mut self, index: usize, cx: &mut Context<Self>) {
        let key = self.scope_key();
        if let Phase::Ready(ready) = &self.phase
            && let Some(row) = ready.rows.get(index)
        {
            let path = row.path.clone();
            let ticked = self.viewed.toggle(key.as_deref(), &path);
            if self.unviewed_only && ticked && ready.selected == Some(index) {
                self.next_unviewed(cx);
            }
            cx.notify();
        }
    }

    /// Advance in file order, wrapping to the first remaining file.
    fn unviewed_after_selection(&self) -> Option<usize> {
        let Phase::Ready(ready) = &self.phase else {
            return None;
        };
        let key = self.scope_key();
        let is_unviewed = |index: &usize| !self.viewed.is(key.as_deref(), &ready.rows[*index].path);
        let start = ready.selected.map_or(0, |index| index + 1);
        (start..ready.rows.len())
            .chain(0..ready.selected.unwrap_or(0))
            .find(is_unviewed)
    }

    fn next_unviewed(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.unviewed_after_selection() {
            self.select_file(index, cx);
        } else if self.unviewed_only {
            self.selected_note = None;
            self.files_ready(cx);
        }
    }

    fn filter_files(&mut self, unviewed_only: bool, cx: &mut Context<Self>) {
        self.unviewed_only = unviewed_only;
        let key = self.scope_key();
        if let Phase::Ready(ready) = &self.phase
            && !ready.listing
            && (ready.selected.is_none()
                || unviewed_only
                    && ready.selected.is_some_and(|index| {
                        self.viewed.is(key.as_deref(), &ready.rows[index].path)
                    }))
        {
            if unviewed_only {
                self.next_unviewed(cx);
            } else {
                self.files_ready(cx);
            }
        }
        cx.notify();
    }

    /// Works out added and removed lines for the files of the list, off the
    /// UI thread, and fills them in when done.
    fn load_counts(&mut self, cx: &mut Context<Self>) {
        let (Some(source), Phase::Ready(ready)) = (self.source(), &self.phase) else {
            return;
        };
        let files: Vec<String> = ready
            .rows
            .iter()
            .take(COUNTED_FILES)
            .map(|row| row.path.clone())
            .collect();
        let listing = self.listings;
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let counts: Vec<Option<(u32, u32)>> = cx
                .background_executor()
                .spawn(async move {
                    files
                        .iter()
                        .map(|path| {
                            let sides = source.sides(&paths, &RepoPath(path.clone())).ok()?;
                            let data = diff_view::compute(
                                path,
                                sides.old.as_deref().unwrap_or(""),
                                sides.new.as_deref().unwrap_or(""),
                            );
                            Some((data.diff.stats.added, data.diff.stats.removed))
                        })
                        .collect()
                })
                .await;
            this.update(cx, |this, cx| {
                if this.listings != listing {
                    return;
                }
                if let Phase::Ready(ready) = &mut this.phase {
                    for (row, stats) in ready.rows.iter_mut().zip(counts) {
                        row.stats = stats;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reads, diffs and highlights file `index` of the current list.
    fn load_diff(&mut self, index: usize, cx: &mut Context<Self>) {
        let (Some(source), Phase::Ready(ready)) = (self.source(), &self.phase) else {
            return;
        };
        let Some(row) = ready.rows.get(index) else {
            return;
        };
        let path = RepoPath(row.path.clone());
        self.shown = None;
        self.diff
            .update(cx, |diff, cx| diff.set_content(DiffContent::Loading, cx));
        self.requests += 1;
        let request = self.requests;
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        match source.sides(&paths, &path) {
                            Ok(sides) => Ok(diff_view::compute(
                                &path.0,
                                sides.old.as_deref().unwrap_or(""),
                                sides.new.as_deref().unwrap_or(""),
                            )),
                            Err(InputsError::NotText(reason)) => {
                                Err(format!("{reason}. There is no text diff to show."))
                            }
                            Err(InputsError::Moved) => {
                                Err("The repository changed. Try again.".into())
                            }
                            Err(InputsError::Read(error)) => {
                                Err(format!("Could not read {}: {error}", path.0))
                            }
                        }
                    }
                })
                .await;
            let texts = this
                .update(cx, |this, cx| {
                    if this.requests != request {
                        return None;
                    }
                    match loaded {
                        Ok(data) => {
                            let texts = (data.diff.old.clone(), data.diff.new.clone());
                            this.shown = Some((data.path.to_string(), data.diff.new.clone()));
                            this.diff.update(cx, |diff, cx| {
                                diff.set_content(DiffContent::Text(Box::new(data)), cx)
                            });
                            Some(texts)
                        }
                        Err(message) => {
                            this.diff.update(cx, |diff, cx| {
                                diff.set_content(DiffContent::Message(message.into()), cx)
                            });
                            this.shown = None;
                            None
                        }
                    }
                })
                .ok()
                .flatten();
            let Some((old, new)) = texts else { return };
            let name = path.0.clone();
            let spans = cx
                .background_executor()
                .spawn(async move { diff_view::highlight_sides(&name, &old, &new) })
                .await;
            this.update(cx, |this, cx| {
                if this.requests == request {
                    this.diff
                        .update(cx, |diff, cx| diff.set_spans(spans.0, spans.1, cx));
                }
            })
            .ok();
        })
        .detach();
    }

    /// Reads the notes of this repo, finds again those whose commit was
    /// rewritten (a rebase) by the text of their line, and keeps the notes
    /// that belong to this review.
    fn refresh_notes(&mut self, cx: &mut Context<Self>) {
        let Phase::Ready(ready) = &self.phase else {
            return;
        };
        let session = self.session.read(cx);
        let paths = session.discovered.paths.clone();
        let repo = notes_db::repo_key(&paths.workdir);
        let branch = sync::plan(&session.state)
            .and_then(|plan| plan.local)
            .map(|name| name.0)
            .unwrap_or_default();
        let commits: Vec<String> = ready.out.commits.iter().map(|c| c.id.clone()).collect();
        let (head, base) = (ready.out.head.clone(), ready.out.base.clone());
        let files: Vec<String> = ready.out.files.iter().map(|f| f.path.clone()).collect();
        let (db, read) = (self.db.clone(), self.reads);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut all = db.notes(&repo)?;
                    let moved: Vec<(NoteId, invoke_core::notes::Anchor)> =
                        review::adoptable(&all, &branch, &commits, &head, &files)
                            .into_iter()
                            .map(|note| (note.id, note.anchor.clone()))
                            .collect();
                    if let Some(tip) = parse(&head) {
                        let from = base.as_deref().and_then(parse);
                        let mut changed = false;
                        for (id, anchor) in moved {
                            let path = RepoPath(anchor.path.clone());
                            let found = read_range_sides(&paths, from.as_ref(), &tip, &path)
                                .ok()
                                .and_then(|sides| sides.new)
                                .and_then(|text| relocate(&anchor, &text));
                            if let Some(line) = found {
                                changed |= db.reanchor(&repo, id, &head, line)?;
                            }
                        }
                        if changed {
                            all = db.notes(&repo)?;
                        }
                    }
                    all.retain(|note| review::is_in_review(note, &commits, &head));
                    Ok::<_, String>(all)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.reads != read {
                    return;
                }
                this.notes_loading = false;
                match result {
                    Ok(notes) => {
                        this.notes = notes;
                        this.notes_error = None;
                    }
                    Err(error) => this.notes_error = Some(format!("Notes: {error}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Adds the note of the form to the line it names, on the file on show.
    fn add_note(&mut self, cx: &mut Context<Self>) {
        let (Some((path, text)), Phase::Ready(ready)) = (self.shown.clone(), &self.phase) else {
            return;
        };
        let commit = self.scope_key().unwrap_or_else(|| ready.out.head.clone());
        let anchor = match review::new_anchor(&commit, &path, &text, self.note_line.read(cx).text())
        {
            Ok(anchor) => anchor,
            Err(problem) => {
                self.note_problem = Some(problem);
                cx.notify();
                return;
            }
        };
        let body = self.note_text.read(cx).text().to_owned();
        if let Err(problem) = invoke_core::notes::clean_body(&body) {
            self.note_problem = Some(review::problem_text(&problem));
            cx.notify();
            return;
        }
        let session = self.session.read(cx);
        let repo = notes_db::repo_key(&session.discovered.paths.workdir);
        let branch = sync::plan(&session.state)
            .and_then(|plan| plan.local)
            .map(|name| name.0)
            .unwrap_or_default();
        let (db, todo) = (self.db.clone(), self.note_todo);
        self.note_problem = None;
        cx.spawn(async move |this, cx| {
            let added = cx
                .background_executor()
                .spawn(async move { db.add(&repo, &branch, &anchor, &body, todo) })
                .await;
            this.update(cx, |this, cx| {
                match added {
                    Ok(_) => {
                        this.note_text
                            .update(cx, |field, cx| field.set_text("", cx));
                        this.note_todo = false;
                        this.refresh_notes(cx);
                    }
                    Err(error) => {
                        this.note_problem = Some(format!("Could not save the note: {error}"))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Resolves, reopens or deletes a note, then reads the notes again.
    fn change_note(&mut self, id: NoteId, change: NoteChange, cx: &mut Context<Self>) {
        let repo = notes_db::repo_key(&self.session.read(cx).discovered.paths.workdir);
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move {
                    match change {
                        NoteChange::Resolve(resolved) => db.set_resolved(&repo, id, resolved),
                        NoteChange::Delete => db.delete(&repo, id),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                match done {
                    Ok(_) => this.refresh_notes(cx),
                    Err(error) => this.notes_error = Some(format!("Notes: {error}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the combined diff for tip notes when possible, or their commit's diff.
    fn open_note(&mut self, id: NoteId, cx: &mut Context<Self>) {
        let (Phase::Ready(ready), Some(note)) =
            (&self.phase, self.notes.iter().find(|note| note.id == id))
        else {
            return;
        };
        let path = note.anchor.path.clone();
        let combined = note.anchor.commit == ready.out.head
            && ready.out.files.iter().any(|file| file.path == path);
        if !combined
            && let Some(index) = ready
                .out
                .commits
                .iter()
                .position(|commit| commit.id == note.anchor.commit)
        {
            self.unviewed_only = false;
            self.select_commit(index, Some(id), cx);
            return;
        }
        self.unviewed_only = false;
        self.set_scope(Scope::Combined, cx);
        self.selected_note = Some(id);
        self.files_ready(cx);
        cx.notify();
    }

    fn next_unresolved_note(&mut self, cx: &mut Context<Self>) {
        let start = self
            .notes
            .iter()
            .position(|note| Some(note.id) == self.selected_note)
            .map_or(0, |index| index + 1);
        if let Some(note) = self.notes[start..]
            .iter()
            .chain(&self.notes[..start])
            .find(|note| note.todo && !note.resolved)
        {
            self.open_note(note.id, cx);
        }
    }

    fn reviewed_push(&self, cx: &Context<Self>) -> Option<Event> {
        let state = &self.session.read(cx).state;
        if !matches!(self.phase, Phase::Ready(_))
            || self.notes_loading
            || state.reads.freshness != Freshness::Fresh
            || state.ops.running.is_some()
            || !state.ops.queued.is_empty()
            || state.snapshot.disk()?.lock != LockState::Free
            || review::key_of(state) != self.loaded
        {
            return None;
        }
        sync::plan(state)
            .filter(|plan| plan.behind() == 0 && plan.clean)
            .and_then(|plan| plan.push(false))
    }

    /// Submit through the normal guarded push flow.
    fn push(&mut self, cx: &mut Context<Self>) {
        if let Some(event) = self.reviewed_push(cx)
            && let Some(key) = self.loaded.clone()
        {
            self.push_request = Some((self.session.read(cx).state.ops.next, key));
            self.session
                .update(cx, |session, cx| session.deliver(event, cx));
        }
    }
}

fn rows_of(files: &[shell::commit_detail::ChangedFile]) -> Vec<Row> {
    files
        .iter()
        .map(|file| Row {
            path: file.path.clone(),
            change: file.change,
            stats: None,
        })
        .collect()
}

impl Render for ReviewPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let back = |cx: &mut Context<Self>| {
            let this = cx.entity();
            Button::new("review-back", "Back to changes")
                .on_click(move |_, _, cx| this.update(cx, |_, cx| cx.emit(ReviewEvent::Back)))
        };
        let centered = |text: String, extra: Option<gpui::AnyElement>, cx: &mut Context<Self>| {
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .items_center()
                .justify_center()
                .gap(px(SPACE_3))
                .bg(hsla(p.bg_app))
                .text_style(&TEXT_BODY, cx)
                .text_color(hsla(p.ink_muted))
                .child(text)
                .children(extra)
                .into_any_element()
        };
        let plan = sync::plan(&self.session.read(cx).state);
        let ready = match &self.phase {
            Phase::Idle | Phase::Loading => {
                return centered("Reading what would be pushed…".into(), None, cx);
            }
            Phase::Failed(error) => {
                let button = back(cx).into_any_element();
                return centered(error.clone(), Some(button), cx);
            }
            Phase::Ready(ready) => ready,
        };
        let out = &ready.out;
        let count = out.commits.len();
        let key = self.loaded.as_ref().expect("a ready review has a range");
        let target = sync::remote_text(&key.target);
        let creates_branch = key.target_tip.is_none();
        if count == 0 && !creates_branch && !key.sets_upstream {
            let has_upstream = plan
                .as_ref()
                .is_some_and(|p| p.local.is_some() && p.upstream.is_some());
            let pr_this = cx.entity();
            let mut actions = div().flex().gap(px(SPACE_2)).child(back(cx));
            if has_upstream && self.can_create_request {
                actions = actions.child(
                    Button::new("review-create-pr", "Create Pull / Merge Request")
                        .primary()
                        .on_click(move |_, _, cx| {
                            pr_this.update(cx, |_, cx| cx.emit(ReviewEvent::CreatePr))
                        }),
                );
            }
            return centered(
                if self.pushed.as_ref().is_some_and(|pushed| {
                    pushed.head == key.head
                        && pushed.local == key.local
                        && pushed.target == key.target
                }) {
                    format!("Pushed to {target}.")
                } else {
                    format!("Nothing to push. This branch has no commits to send to {target}.")
                },
                Some(actions.into_any_element()),
                cx,
            );
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();
        let behind = plan.as_ref().map_or(0, sync::Plan::behind);
        let subtitle = review::subtitle(&key.local.0, &target, behind, creates_branch);
        let push_action = review::push_label(count, out.truncated, creates_branch);
        let scope_key = self.scope_key();
        let scope = self.scope;
        let selected_commit = self.commit;

        let this = cx.entity();
        let scope_switch = Segmented::new(
            "review-scope",
            vec!["Combined".into(), "Per commit".into()],
            match scope {
                Scope::Combined => 0,
                Scope::PerCommit => 1,
            },
            move |index, _, cx| {
                this.update(cx, |pane, cx| {
                    pane.set_scope(
                        if index == 0 {
                            Scope::Combined
                        } else {
                            Scope::PerCommit
                        },
                        cx,
                    )
                })
            },
        );
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(SPACE_3))
            .px(px(SPACE_4))
            .py(px(SPACE_2))
            .bg(hsla(p.bg_panel))
            .border_b_1()
            .border_color(hsla(p.line))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w(px(0.))
                    .child(
                        div()
                            .text_style(&TEXT_TITLE, cx)
                            .text_color(hsla(p.ink))
                            .child(if count == 0 {
                                "Review branch before pushing".to_owned()
                            } else {
                                review::title(count, out.truncated)
                            }),
                    )
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(subtitle),
                    ),
            )
            .child(div().flex_1())
            .child(scope_switch);

        // The outgoing commits.
        let mut commits = div().flex().flex_col().gap(px(SPACE_1)).child(
            div()
                .px(px(SPACE_2))
                .text_style(&TEXT_LABEL, cx)
                .text_color(hsla(p.ink_muted))
                .child(format!(
                    "OUTGOING COMMITS · {}",
                    if out.truncated {
                        format!("{count}+")
                    } else {
                        count.to_string()
                    }
                )),
        );
        for (index, commit) in out.commits.iter().enumerate() {
            let chosen = scope == Scope::PerCommit && index == selected_commit;
            let this = cx.entity();
            let hover = hsla(p.bg_hover);
            let commit_id = ElementId::from(("review-commit", index));
            let handle = focus::handle(&commit_id, window, cx);
            let mut row = div()
                .id(commit_id)
                .track_focus(&handle)
                .tab_stop(true)
                .relative()
                .flex()
                .flex_col()
                .px(px(SPACE_2))
                .py(px(SPACE_2))
                .rounded(px(RADIUS_MD))
                .cursor_pointer()
                .focus_visible(move |style| focus::ring(style, ring))
                .on_click(move |_, _, cx| {
                    this.update(cx, |pane, cx| pane.select_commit(index, None, cx))
                })
                .child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .text_style(&TEXT_BODY, cx)
                        .text_color(hsla(p.ink))
                        .child(commit.subject.clone()),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(SPACE_2))
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(
                            div()
                                .text_style(&TEXT_CODE_SMALL, cx)
                                .child(short_hash(&commit.id).to_owned()),
                        )
                        .child(relative(now, commit.seconds))
                        .children(commit.is_merge.then_some("merge")),
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
            commits = commits.child(row);
        }

        // The files of the diff on show.
        let done = self.viewed.count(
            scope_key.as_deref(),
            ready.rows.iter().map(|r| r.path.as_str()),
        );
        let (filter_this, next_file) = (cx.entity(), cx.entity());
        let mut files = div()
            .flex()
            .flex_col()
            .gap(px(SPACE_1))
            .child(
                div()
                    .px(px(SPACE_2))
                    .text_style(&TEXT_LABEL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(format!(
                        "FILES · {}",
                        review::viewed_text(done, ready.rows.len())
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(SPACE_2))
                    .child(Segmented::new(
                        "review-file-filter",
                        vec!["All files".into(), "Unviewed".into()],
                        usize::from(self.unviewed_only),
                        move |index, _, cx| {
                            filter_this.update(cx, |pane, cx| pane.filter_files(index == 1, cx))
                        },
                    ))
                    .child(
                        Button::new("review-next-unviewed", "Next unviewed")
                            .compact()
                            .disabled(self.unviewed_after_selection().is_none())
                            .on_click(move |_, _, cx| {
                                next_file.update(cx, |pane, cx| pane.next_unviewed(cx))
                            }),
                    ),
            );
        if ready.listing {
            files = files.child(
                div()
                    .px(px(SPACE_2))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child("Reading the files…"),
            );
        } else if self.unviewed_only && done == ready.rows.len() {
            files = files.child(
                div()
                    .px(px(SPACE_2))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child("No unviewed files in this diff."),
            );
        }
        for (index, row) in ready.rows.iter().enumerate() {
            let chosen = ready.selected == Some(index);
            let ticked = self.viewed.is(scope_key.as_deref(), &row.path);
            if self.unviewed_only && ticked {
                continue;
            }
            let (select, tick) = (cx.entity(), cx.entity());
            let hover = hsla(p.bg_hover);
            let file_id = ElementId::from(("review-file", index));
            let handle = focus::handle(&file_id, window, cx);
            let mut line = div()
                .id(file_id)
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
                .on_click(move |_, _, cx| select.update(cx, |pane, cx| pane.select_file(index, cx)))
                .child(
                    Checkbox::new(
                        ("review-viewed", index),
                        if ticked {
                            CheckState::Checked
                        } else {
                            CheckState::Unchecked
                        },
                    )
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        tick.update(cx, |pane, cx| pane.toggle_viewed(index, cx))
                    }),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(LANE))
                        .text_style(&TEXT_CODE_SMALL, cx)
                        .text_color(hsla(match row.change {
                            Change::Added => p.add_ink,
                            Change::Deleted => p.remove_ink,
                            _ => p.warning,
                        }))
                        .child(change_letter(row.change)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_style(&TEXT_BODY, cx)
                        .text_color(hsla(p.ink))
                        .child(SharedString::from(row.path.clone())),
                );
            if let Some((added, removed)) = review::counts(row.stats) {
                line = line
                    .child(
                        div()
                            .flex_none()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.add_ink))
                            .child(added),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.remove_ink))
                            .child(removed),
                    );
            }
            line = if chosen {
                line.bg(hsla(p.bg_raised)).child(
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
                line.hover(move |style| style.bg(hover))
            };
            files = files.child(line);
        }

        // The notes of this review.
        let open = open_todos(&self.notes);
        let mut notes = div().flex().flex_col().gap(px(SPACE_1)).child(
            div()
                .px(px(SPACE_2))
                .text_style(&TEXT_LABEL, cx)
                .text_color(hsla(p.ink_muted))
                .child(format!("NOTES · {}", self.notes.len())),
        );
        if let Some(error) = &self.notes_error {
            notes = notes.child(
                div()
                    .px(px(SPACE_2))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.warning))
                    .child(error.clone()),
            );
        }
        for (index, note) in self.notes.iter().enumerate() {
            let (open_it, change) = (cx.entity(), cx.entity());
            let hover = hsla(p.bg_hover);
            let (id, todo, resolved) = (note.id, note.todo, note.resolved);
            let mut head = div().flex().items_center().gap(px(SPACE_2));
            if todo {
                let (text, tone) = if resolved {
                    ("Resolved", PillTone::Neutral)
                } else {
                    ("To-do", PillTone::Warning)
                };
                head = head.child(Pill::new(text).tone(tone).compact());
            }
            head = head.child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(review::place(note)),
            );
            let mut buttons = div().flex().gap(px(SPACE_1));
            if todo {
                let toggle = cx.entity();
                buttons = buttons.child(
                    Button::new(
                        ("review-note-resolve", index),
                        if resolved { "Reopen" } else { "Resolve" },
                    )
                    .ghost()
                    .compact()
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        toggle.update(cx, |pane, cx| {
                            pane.change_note(id, NoteChange::Resolve(!resolved), cx)
                        })
                    }),
                );
            }
            buttons = buttons.child(
                Button::new(("review-note-delete", index), "Delete")
                    .ghost()
                    .compact()
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        change.update(cx, |pane, cx| pane.change_note(id, NoteChange::Delete, cx))
                    }),
            );
            let note_id = ElementId::from(("review-note", index));
            let handle = focus::handle(&note_id, window, cx);
            notes = notes.child(
                div()
                    .id(note_id)
                    .track_focus(&handle)
                    .tab_stop(true)
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_1))
                    .px(px(SPACE_2))
                    .py(px(SPACE_2))
                    .rounded(px(RADIUS_MD))
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
                    .focus_visible(move |style| focus::ring(style, ring))
                    .on_click(move |_, _, cx| open_it.update(cx, |pane, cx| pane.open_note(id, cx)))
                    .child(head)
                    .child(
                        div()
                            .max_h(px(60.))
                            .overflow_hidden()
                            .text_style(&TEXT_BODY, cx)
                            .text_color(hsla(p.ink))
                            .child(note.body.clone()),
                    )
                    .child(buttons),
            );
        }

        let aside = div()
            .id("review-aside")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(self.aside_w))
            .gap(px(SPACE_4))
            .p(px(SPACE_2))
            .overflow_y_scroll()
            .bg(hsla(p.bg_panel))
            .child(files)
            .child(notes)
            .child(commits);

        // The form for a new note, on the file on show.
        let form = self.shown.as_ref().map(|(path, _)| {
            let (add, todo) = (cx.entity(), cx.entity());
            let mut form = div()
                .flex()
                .flex_col()
                .flex_none()
                .gap(px(SPACE_2))
                .p(px(SPACE_3))
                .border_t_1()
                .border_color(hsla(p.line))
                .bg(hsla(p.bg_panel))
                .child(
                    div()
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(format!(
                            "Add a note on {path}. The line is counted on the new side of this diff."
                        )),
                )
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(SPACE_2))
                        .child(div().flex_none().w(px(96.)).child(self.note_line.clone()))
                        .child(div().flex_1().min_w(px(0.)).child(self.note_text.clone()))
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .flex_col()
                                .gap(px(SPACE_2))
                                .child(
                                    Checkbox::new(
                                        "review-note-todo",
                                        if self.note_todo {
                                            CheckState::Checked
                                        } else {
                                            CheckState::Unchecked
                                        },
                                    )
                                    .label("To-do")
                                    .on_click(move |_, _, cx| {
                                        todo.update(cx, |pane, cx| {
                                            pane.note_todo = !pane.note_todo;
                                            cx.notify();
                                        })
                                    }),
                                )
                                .child(Button::new("review-note-add", "Add note").on_click(
                                    move |_, _, cx| add.update(cx, |pane, cx| pane.add_note(cx)),
                                )),
                        ),
                );
            if let Some(problem) = &self.note_problem {
                form = form.child(
                    div()
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.warning))
                        .child(problem.clone()),
                );
            }
            form
        });

        let note_location = self.selected_note.and_then(|id| {
            let note = self.notes.iter().find(|note| note.id == id)?;
            let matching_file = ready
                .selected
                .and_then(|index| ready.rows.get(index))
                .is_some_and(|row| row.path == note.anchor.path);
            Some(
                div()
                    .flex_none()
                    .max_h(px(ROW_LIST * 3.0))
                    .overflow_hidden()
                    .px(px(SPACE_3))
                    .py(px(SPACE_2))
                    .border_b_1()
                    .border_color(hsla(p.line))
                    .bg(hsla(p.bg_panel))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink))
                    .child(if ready.listing || matching_file {
                        format!("Note on {} · {}", review::place(note), note.body)
                    } else {
                        format!(
                            "The file for the note on {} is unavailable in this diff.",
                            review::place(note)
                        )
                    }),
            )
        });
        let main = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .children(note_location)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .bg(hsla(p.bg_panel))
                    .child(self.diff.clone()),
            )
            .children(form);

        let (push_this, next_note) = (cx.entity(), cx.entity());
        let can_push = self.reviewed_push(cx).is_some();
        let push_scope = if count > 0 {
            format!("All outgoing commits to {target}")
        } else if creates_branch {
            format!("Create the remote branch {target}")
        } else {
            format!("Set {target} as upstream")
        };
        let footer = div()
            .flex()
            .flex_wrap()
            .flex_none()
            .items_center()
            .gap(px(SPACE_3))
            .px(px(SPACE_4))
            .py(px(SPACE_2))
            .border_t_1()
            .border_color(hsla(p.line))
            .bg(hsla(p.bg_panel))
            .children(
                review::todo_warning(open)
                    .map(|text| Pill::new(text).tone(PillTone::Warning).compact()),
            )
            .children((open > 0).then(|| {
                Button::new("review-next-open-note", "Next to-do")
                    .compact()
                    .on_click(move |_, _, cx| {
                        next_note.update(cx, |pane, cx| pane.next_unresolved_note(cx))
                    })
            }))
            .children(self.notes_loading.then(|| {
                div()
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child("Loading review notes…")
            }))
            .child(
                div()
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(push_scope),
            )
            .child(div().flex_1())
            .child(back(cx))
            .child(
                Button::new("review-push", push_action)
                    .primary()
                    .disabled(!can_push)
                    .on_click(move |_, _, cx| push_this.update(cx, |pane, cx| pane.push(cx))),
            );

        let resize_aside = cx.entity();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(hsla(p.bg_app))
            .child(header)
            .children(self.range_changed.then(|| {
                div()
                    .flex_none()
                    .px(px(SPACE_4))
                    .py(px(SPACE_2))
                    .bg(hsla(p.warning_soft))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.warning))
                    .child("The outgoing range or destination changed. Viewed progress was reset.")
            }))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(aside)
                    .child(ResizeHandle::new(
                        "review-aside-resize",
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
            .child(footer)
            .into_any_element()
    }
}
