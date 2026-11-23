//! The pre-push review screen (task 21): the commits that would leave the
//! machine, the files they change as one combined diff or one commit at a
//! time, "viewed" ticks, and the Push button. It reads through
//! `shell::outgoing` on a background thread, reads again whenever the head or
//! the upstream moves (a fetch, a commit, a push), and sends nothing itself:
//! Push is a `PushRequested` like the header's.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};
use invoke_core::RepoPath;
use shell::InputsError;
use shell::commit_detail::{Change, CommitSides, read_commit, read_commit_sides};
use shell::history::CommitId;
use shell::outgoing::{Outgoing, read_outgoing, read_range_sides};
use shell::precondition::RepoPaths;
use tokens::{
    RADIUS_MD, SPACE_1, SPACE_2, SPACE_3, SPACE_4, TEXT_BODY, TEXT_CODE_SMALL, TEXT_LABEL,
    TEXT_SMALL, TEXT_TITLE,
};
use ui::convert::hsla;
use ui::time::relative;
use ui::{
    Button, CheckState, Checkbox, DiffContent, DiffView, Segmented, TextStyleExt, ThemeExt,
    diff_view, short_hash,
};

use crate::review::{self, Key, Scope, Viewed};
use crate::session::RepoSession;
use crate::sync;

/// Files whose line counts are worked out for the list; a bigger list shows
/// counts for the first ones only.
const COUNTED_FILES: usize = 300;

pub enum ReviewEvent {
    /// Leave the review for the Changes page.
    Back,
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

pub struct ReviewPane {
    session: Entity<RepoSession>,
    diff: Entity<DiffView>,
    phase: Phase,
    scope: Scope,
    /// The commit shown in [`Scope::PerCommit`] (index into the list).
    commit: usize,
    viewed: Viewed,
    /// On screen: only then does a moved head or upstream cause a re-read.
    active: bool,
    loaded: Option<Key>,
    /// Numbers each read of the outgoing list; an older answer is dropped.
    reads: u64,
    /// Numbers each file list and its counts.
    listings: u64,
    /// Numbers each diff load.
    requests: u64,
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
            active: false,
            loaded: None,
            reads: 0,
            listings: 0,
            requests: 0,
            _observe: observe,
        }
    }

    /// The review was opened: read what would be pushed, now.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.active = true;
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
            let now = review::key_of(&self.session.read(cx).state);
            if now.is_some() && now != self.loaded {
                self.reload(cx);
                return;
            }
        }
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let state = &self.session.read(cx).state;
        self.loaded = review::key_of(state);
        let upstream = sync::plan(state)
            .and_then(|plan| plan.upstream)
            .map(|upstream| upstream.tracking_ref().0);
        self.reads += 1;
        let read = self.reads;
        self.viewed.clear();
        self.scope = Scope::Combined;
        self.commit = 0;
        self.phase = Phase::Loading;
        self.diff
            .update(cx, |diff, cx| diff.set_content(DiffContent::Loading, cx));
        cx.notify();
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { read_outgoing(&paths, upstream.as_deref()) })
                .await;
            this.update(cx, |this, cx| {
                if this.reads != read {
                    return;
                }
                match result {
                    Ok(out) => {
                        let rows = rows_of(&out.files);
                        this.phase = Phase::Ready(Box::new(Ready {
                            out,
                            rows,
                            listing: false,
                            selected: None,
                        }));
                        this.files_ready(cx);
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

    /// The file list is in place: count its lines and show the first file.
    fn files_ready(&mut self, cx: &mut Context<Self>) {
        self.listings += 1;
        let first = match &mut self.phase {
            Phase::Ready(ready) => {
                ready.selected = (!ready.rows.is_empty()).then_some(0);
                ready.selected
            }
            _ => return,
        };
        self.load_counts(cx);
        match first {
            Some(index) => self.load_diff(index, cx),
            None => {
                self.requests += 1;
                self.diff.update(cx, |diff, cx| {
                    diff.set_content(DiffContent::Message("No file to show.".into()), cx)
                });
            }
        }
    }

    fn set_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        if self.scope == scope && !matches!(scope, Scope::PerCommit) {
            return;
        }
        self.scope = scope;
        match scope {
            Scope::Combined => {
                if let Phase::Ready(ready) = &mut self.phase {
                    ready.rows = rows_of(&ready.out.files);
                }
                self.files_ready(cx);
            }
            Scope::PerCommit => self.select_commit(self.commit, cx),
        }
        cx.notify();
    }

    /// Shows commit `index` on its own: its files and the first one's diff.
    fn select_commit(&mut self, index: usize, cx: &mut Context<Self>) {
        let Phase::Ready(ready) = &mut self.phase else {
            return;
        };
        let Some(commit) = ready.out.commits.get(index) else {
            return;
        };
        let Some(id) = parse(&commit.id) else { return };
        self.scope = Scope::PerCommit;
        self.commit = index;
        ready.rows.clear();
        ready.listing = true;
        ready.selected = None;
        self.listings += 1;
        let listing = self.listings;
        self.requests += 1;
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
        self.diff
            .update(cx, |diff, cx| diff.set_content(DiffContent::Loading, cx));
        self.load_diff(index, cx);
        cx.notify();
    }

    fn toggle_viewed(&mut self, index: usize, cx: &mut Context<Self>) {
        let key = self.scope_key();
        if let Phase::Ready(ready) = &self.phase
            && let Some(row) = ready.rows.get(index)
        {
            let path = row.path.clone();
            self.viewed.toggle(key.as_deref(), &path);
            cx.notify();
        }
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
                            this.diff.update(cx, |diff, cx| {
                                diff.set_content(DiffContent::Text(Box::new(data)), cx)
                            });
                            Some(texts)
                        }
                        Err(message) => {
                            this.diff.update(cx, |diff, cx| {
                                diff.set_content(DiffContent::Message(message.into()), cx)
                            });
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

    /// Push, as the header's Push does; the strip under the header shows it
    /// going, and the list is read again when the upstream moves.
    fn push(&mut self, cx: &mut Context<Self>) {
        let event = sync::plan(&self.session.read(cx).state).and_then(|plan| plan.push(false));
        if let Some(event) = event {
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
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
        let busy = self.session.read(cx).state.ops.running.is_some();
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
        if count == 0 {
            let target = plan
                .as_ref()
                .and_then(sync::Plan::push_target)
                .map(|target| sync::remote_text(&target))
                .unwrap_or_else(|| "the remote".to_owned());
            let button = back(cx).into_any_element();
            return centered(
                format!("Nothing to push. This branch has no commits that {target} lacks."),
                Some(button),
                cx,
            );
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();
        let publishes = plan.as_ref().is_some_and(sync::Plan::publishes);
        let subtitle = match (&plan, plan.as_ref().and_then(sync::Plan::push_target)) {
            (Some(plan), Some(target)) => review::subtitle(
                plan.local.as_ref().map_or("", |name| name.0.as_str()),
                &sync::remote_text(&target),
                plan.behind(),
                publishes,
            ),
            _ => String::new(),
        };
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
                            .child(review::title(count, out.truncated)),
                    )
                    .child(
                        div()
                            .truncate()
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
            let mut row = div()
                .id(("review-commit", index))
                .flex()
                .flex_col()
                .px(px(SPACE_2))
                .py(px(SPACE_1 + 2.0))
                .rounded(px(RADIUS_MD))
                .cursor_pointer()
                .on_click(move |_, _, cx| this.update(cx, |pane, cx| pane.select_commit(index, cx)))
                .child(
                    div()
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
                row.bg(hsla(p.bg_raised))
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
        let mut files = div().flex().flex_col().gap(px(SPACE_1)).child(
            div()
                .flex()
                .justify_between()
                .px(px(SPACE_2))
                .text_style(&TEXT_LABEL, cx)
                .text_color(hsla(p.ink_muted))
                .child(format!("FILES · {}", ready.rows.len()))
                .child(format!("{done} OF {} VIEWED", ready.rows.len())),
        );
        if ready.listing {
            files = files.child(
                div()
                    .px(px(SPACE_2))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child("Reading the files…"),
            );
        }
        for (index, row) in ready.rows.iter().enumerate() {
            let chosen = ready.selected == Some(index);
            let ticked = self.viewed.is(scope_key.as_deref(), &row.path);
            let (select, tick) = (cx.entity(), cx.entity());
            let hover = hsla(p.bg_hover);
            let mut line = div()
                .id(("review-file", index))
                .flex()
                .items_center()
                .gap(px(SPACE_2))
                .h(px(28.))
                .px(px(SPACE_2))
                .rounded(px(RADIUS_MD))
                .cursor_pointer()
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
                        tick.update(cx, |pane, cx| pane.toggle_viewed(index, cx))
                    }),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(14.))
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
                line.bg(hsla(p.bg_raised))
            } else {
                line.hover(move |style| style.bg(hover))
            };
            files = files.child(line);
        }

        let aside = div()
            .id("review-aside")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(340.))
            .gap(px(SPACE_4))
            .p(px(SPACE_2))
            .overflow_y_scroll()
            .bg(hsla(p.bg_panel))
            .border_r_1()
            .border_color(hsla(p.line))
            .child(commits)
            .child(files);

        let main = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .bg(hsla(p.bg_panel))
                    .child(self.diff.clone()),
            );

        let total = ready.rows.len();
        let push_this = cx.entity();
        let can_push = plan.as_ref().is_some_and(sync::Plan::can_push) && !busy;
        let footer = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(SPACE_3))
            .px(px(SPACE_4))
            .py(px(SPACE_2))
            .border_t_1()
            .border_color(hsla(p.line))
            .bg(hsla(p.bg_panel))
            .child(
                div()
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(review::viewed_text(done, total)),
            )
            .child(div().flex_1())
            .child(back(cx))
            .child(
                Button::new(
                    "review-push",
                    review::push_label(count, out.truncated, publishes),
                )
                .primary()
                .disabled(!can_push)
                .on_click(move |_, _, cx| push_this.update(cx, |pane, cx| pane.push(cx))),
            );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(hsla(p.bg_app))
            .child(header)
            .child(div().flex().flex_1().min_h(px(0.)).child(aside).child(main))
            .child(footer)
            .into_any_element()
    }
}
