//! The History page of a repo tab: the commit graph. It starts the history
//! walk on a background thread, feeds the pages to the graph view, reads the
//! subject, author and date of the rows on screen when the view asks, and
//! starts the walk again when the refs or `HEAD` change (a refresh keeps the
//! old graph on screen until the first new page arrives).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Pixels, Point, Render, Styled, Subscription, Task, Window, anchored, deferred, div, px,
};
use invoke_core::{Head, Oid, RefsSnapshot, Snapshot};
use shell::history::{CommitId, CommitMeta, HistoryEvent, HistoryScope, read_meta, walk_scoped};
use tokens::{SPACE_1, SPACE_2, SPACE_3};
use ui::convert::{hsla, shadows};
use ui::time::relative;
use ui::{
    Button, GraphEvent, GraphView, IconName, ResizeEdge, ResizeHandle, RowMeta, Segmented,
    ThemeExt, ref_labels::ref_labels,
};

use crate::commit_pane::{CommitPane, InspectFile, StartRebase, rebase_button, replay_buttons};
use crate::commit_search::{CommitSearch, CommitSelected};
use crate::session::RepoSession;

pub enum HistoryUiEvent {
    Changes,
    Recovery,
    LayoutChanged,
}

/// The width of the commit detail beside the graph.
const DETAIL_W: f32 = tokens::DIALOG;
const DETAIL_MIN: f32 = tokens::SIDEBAR + tokens::SPACE_8 * 2.0 + tokens::SPACE_4;
const DETAIL_MAX: f32 = tokens::DIALOG + tokens::SIDEBAR + tokens::SPACE_8 * 2.0;

pub struct HistoryPane {
    session: Entity<RepoSession>,
    pub graph: Entity<GraphView<CommitId>>,
    detail: Entity<CommitPane>,
    search: Entity<CommitSearch>,
    cancel: Arc<AtomicBool>,
    started: bool,
    last_refs: Option<(Head, RefsSnapshot)>,
    row_menu: Option<(Oid, Point<Pixels>)>,
    detail_w: f32,
    scope: HistoryScope,
    expanded: bool,
    _walk: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<InspectFile> for HistoryPane {}
impl EventEmitter<StartRebase> for HistoryPane {}
impl EventEmitter<HistoryUiEvent> for HistoryPane {}

/// What a row shows for `meta`, with the date relative to `now`.
pub fn row_meta(meta: &CommitMeta, now: i64) -> RowMeta {
    RowMeta {
        subject: meta.subject.clone().into(),
        author: meta.author.clone().into(),
        date: relative(now, meta.seconds).into(),
    }
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl HistoryPane {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let graph = cx.new(|_| GraphView::<CommitId>::new());
        let detail = cx.new(|cx| CommitPane::new(session.clone(), cx));
        let paths = session.read(cx).discovered.paths.clone();
        let search = cx.new(|cx| CommitSearch::new(paths, cx));
        let subscriptions = vec![
            cx.subscribe(&search, |this, _, event: &CommitSelected, cx| {
                this.set_scope(HistoryScope::AllRefs, cx);
                this.open_commit(event.0, cx)
            }),
            cx.subscribe(&detail, |_, _, event: &InspectFile, cx| {
                cx.emit(InspectFile(event.0.clone(), event.1));
            }),
            cx.subscribe(&detail, |_, _, event: &StartRebase, cx| {
                cx.emit(StartRebase(event.0.clone()));
            }),
            cx.subscribe(&graph, |this, graph, event: &GraphEvent, cx| match event {
                GraphEvent::NeedMeta(range) => this.load_meta(range.clone(), cx),
                GraphEvent::Selected(row) => {
                    this.row_menu = None;
                    let id = graph.read(cx).id_at(*row).copied();
                    this.detail.update(cx, |detail, cx| detail.show(id, cx));
                    cx.notify();
                }
                GraphEvent::RowMenu { row, position } => {
                    this.row_menu = graph
                        .read(cx)
                        .id_at(*row)
                        .and_then(|id| Oid::from_hex(&id.to_string()).ok())
                        .map(|oid| (oid, *position));
                    cx.notify();
                }
            }),
            cx.observe(&session, |this, _, cx| this.session_changed(cx)),
        ];
        Self {
            session,
            graph,
            detail,
            search,
            cancel: Arc::new(AtomicBool::new(false)),
            started: false,
            last_refs: None,
            row_menu: None,
            detail_w: DETAIL_W,
            scope: HistoryScope::AllRefs,
            expanded: false,
            _walk: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn detail_width(&self) -> f32 {
        self.detail_w
    }

    pub fn set_detail_width(&mut self, width: f32, cx: &mut Context<Self>) {
        self.detail_w = width.clamp(DETAIL_MIN, DETAIL_MAX);
        cx.notify();
    }

    pub fn reset_layout(&mut self, cx: &mut Context<Self>) {
        self.expanded = false;
        self.set_detail_width(DETAIL_W, cx);
    }

    fn set_scope(&mut self, scope: HistoryScope, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            self.start_walk(true, cx);
            cx.notify();
        }
    }

    fn head_id(&self, cx: &Context<Self>) -> Option<CommitId> {
        let head = &self.session.read(cx).state.snapshot.disk()?.head;
        match head {
            Head::Branch { oid, .. } | Head::Detached(oid) => {
                CommitId::from_hex(oid.as_hex().as_bytes()).ok()
            }
            Head::Unborn(_) => None,
        }
    }

    pub fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.toggle(window, cx));
        cx.notify();
    }

    /// Starts the walk the first time the page is shown.
    pub fn ensure_started(&mut self, cx: &mut Context<Self>) {
        if self.started {
            return;
        }
        self.started = true;
        self.sync_refs(cx);
        self.start_walk(false, cx);
    }

    pub fn open_commit(&mut self, id: CommitId, cx: &mut Context<Self>) {
        self.graph.update(cx, |graph, cx| graph.reveal(id, cx));
        self.detail
            .update(cx, |detail, cx| detail.show(Some(id), cx));
        cx.notify();
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.set_visible(visible, cx));
        if !visible && self.row_menu.take().is_some() {
            cx.notify();
        }
    }

    /// The refs and `HEAD` of the newest read, if one has landed.
    fn refs(&self, cx: &Context<Self>) -> Option<(Head, RefsSnapshot)> {
        match &self.session.read(cx).state.snapshot {
            Snapshot::Read { disk, .. } => Some((disk.head.clone(), disk.refs.clone())),
            Snapshot::NotRead => None,
        }
    }

    /// Gives the graph its labels and the commit `HEAD` is on.
    fn sync_refs(&mut self, cx: &mut Context<Self>) {
        let Some((head, refs)) = self.refs(cx) else {
            return;
        };
        let head_id = match &head {
            Head::Branch { oid, .. } | Head::Detached(oid) => {
                CommitId::from_hex(oid.as_hex().as_bytes()).ok()
            }
            Head::Unborn(_) => None,
        };
        let labels = ref_labels(&refs, &head);
        self.last_refs = Some((head, refs));
        self.graph
            .update(cx, |graph, cx| graph.set_refs(labels, head_id, cx));
    }

    fn session_changed(&mut self, cx: &mut Context<Self>) {
        self.detail.update(cx, |_, cx| cx.notify());
        cx.notify();
        if !self.started {
            return;
        }
        let Some(now) = self.refs(cx) else { return };
        if self.last_refs.as_ref() != Some(&now) {
            self.row_menu = None;
            self.sync_refs(cx);
            self.start_walk(true, cx);
            self.search.update(cx, |search, cx| search.refresh(cx));
        }
    }

    /// Walks the history on a background thread and feeds the graph.
    fn start_walk(&mut self, refresh: bool, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        let paths = self.session.read(cx).discovered.paths.clone();
        let cancel = self.cancel.clone();
        let inbox = walk_scoped(paths, None, self.scope, cancel.clone());
        self.graph.update(cx, |graph, cx| {
            if refresh {
                graph.begin_refresh();
            } else {
                graph.begin(cx);
            }
        });
        let graph = self.graph.clone();
        let head_hint = self.refs(cx).and_then(|(head, _)| match head {
            Head::Branch { oid, .. } | Head::Detached(oid) => {
                CommitId::from_hex(oid.as_hex().as_bytes()).ok()
            }
            Head::Unborn(_) => None,
        });
        self._walk = Some(cx.spawn(async move |_, cx| {
            let mut inbox = inbox;
            let mut selected_initial = false;
            loop {
                let (back, batch) = cx
                    .background_executor()
                    .spawn(async move {
                        let mut batch = Vec::new();
                        if let Ok(first) = inbox.recv() {
                            batch.push(first);
                            while batch.len() < 8 {
                                match inbox.try_recv() {
                                    Ok(next) => batch.push(next),
                                    Err(_) => break,
                                }
                            }
                        }
                        (inbox, batch)
                    })
                    .await;
                inbox = back;
                if batch.is_empty() || cancel.load(Ordering::Relaxed) {
                    break;
                }
                graph.update(cx, |graph, cx| {
                    for event in batch {
                        match event {
                            HistoryEvent::Started { .. } => {}
                            HistoryEvent::Page(page) => {
                                graph.push_page(&page, cx);
                                if !selected_initial
                                    && graph.selected().is_none()
                                    && let Some(target) =
                                        head_hint.or_else(|| graph.id_at(0).copied())
                                {
                                    graph.reveal(target, cx);
                                    selected_initial = true;
                                }
                            }
                            HistoryEvent::Done { commits } => graph.finish(commits, cx),
                            HistoryEvent::Failed(error) => graph.fail(error, cx),
                        }
                    }
                });
            }
        }));
    }

    /// Reads the metadata of rows `range` and hands it to the graph.
    fn load_meta(&mut self, range: std::ops::Range<usize>, cx: &mut Context<Self>) {
        let cancel = self.cancel.clone();
        let ids: Vec<CommitId> = {
            let graph = self.graph.read(cx);
            range
                .clone()
                .filter_map(|row| graph.id_at(row).copied())
                .collect()
        };
        let paths = self.session.read(cx).discovered.paths.clone();
        let graph = self.graph.clone();
        cx.spawn(async move |_, cx| {
            let read = cx
                .background_executor()
                .spawn(async move { read_meta(&paths, &ids) })
                .await;
            let Ok(metas) = read else { return };
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let now = now_seconds();
            let rows: Vec<Option<RowMeta>> = metas
                .iter()
                .map(|meta| meta.as_ref().map(|m| row_meta(m, now)))
                .collect();
            graph.update(cx, |graph, cx| graph.set_meta(range.start, rows, cx));
        })
        .detach();
    }
}

impl Drop for HistoryPane {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Render for HistoryPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let menu = self.row_menu.clone().map(|(oid, position)| {
            let pane = cx.entity().downgrade();
            let rebase_pane = cx.entity().downgrade();
            let rebase = rebase_button(oid.clone(), self.session.read(cx), move |target, cx| {
                rebase_pane
                    .update(cx, |pane, cx| {
                        pane.row_menu = None;
                        cx.emit(StartRebase(target));
                        cx.notify();
                    })
                    .ok();
            });
            let buttons = replay_buttons(oid, self.session.read(cx), move |event, cx| {
                pane.update(cx, |pane, cx| {
                    pane.row_menu = None;
                    pane.session
                        .update(cx, |session, cx| session.deliver(event, cx));
                    cx.notify();
                })
                .ok();
            });
            deferred(
                anchored().position(position).snap_to_window().child(
                    div()
                        .id("commit-row-menu")
                        .occlude()
                        .flex()
                        .flex_col()
                        .min_w(px(tokens::SIDEBAR - tokens::SPACE_8))
                        .gap(px(tokens::SPACE_1 / 2.0))
                        .p(px(tokens::SPACE_1))
                        .rounded(px(tokens::RADIUS_LG))
                        .border_1()
                        .border_color(hsla(p.line))
                        .bg(hsla(p.bg_raised))
                        .shadow(shadows(p.shadow_overlay))
                        .on_mouse_down_out(cx.listener(|pane, _, _, cx| {
                            pane.row_menu = None;
                            cx.notify();
                        }))
                        .children(buttons.into_iter().map(ui::Button::full_width))
                        .child(rebase.full_width()),
                ),
            )
        });

        let search_open = self.search.read(cx).is_open();
        let changes = self
            .session
            .read(cx)
            .state
            .snapshot
            .disk()
            .map_or(0, |disk| disk.status.entries.len());
        let scope_changed = cx.entity();
        let scope = Segmented::new(
            "history-scope",
            vec!["All refs".into(), "Current branch".into()],
            usize::from(self.scope == HistoryScope::Head),
            move |index, _, cx| {
                let scope = if index == 0 {
                    HistoryScope::AllRefs
                } else {
                    HistoryScope::Head
                };
                scope_changed.update(cx, |pane, cx| pane.set_scope(scope, cx));
            },
        );
        let header = div()
            .flex()
            .flex_wrap()
            .flex_none()
            .items_center()
            .gap(px(SPACE_2))
            .px(px(SPACE_3))
            .py(px(SPACE_1))
            .bg(hsla(p.bg_panel))
            .border_b_1()
            .border_color(hsla(p.line))
            .child(scope)
            .child(
                Button::new(
                    "history-search",
                    if search_open {
                        "Hide search"
                    } else {
                        "Find commit"
                    },
                )
                .icon(IconName::Search)
                .shortcut("Ctrl+F")
                .ghost()
                .on_click(cx.listener(|pane, _, window, cx| pane.toggle_search(window, cx))),
            )
            .child(
                Button::new("history-head", "Jump to HEAD")
                    .ghost()
                    .disabled(self.head_id(cx).is_none())
                    .on_click(cx.listener(|pane, _, _, cx| {
                        if let Some(id) = pane.head_id(cx) {
                            pane.open_commit(id, cx);
                        }
                    })),
            )
            .child(
                Button::new("history-recovery", "Recovery")
                    .ghost()
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(HistoryUiEvent::Recovery))),
            )
            .child(div().flex_1())
            .child(
                Button::new(
                    "history-expand",
                    if self.expanded {
                        "Show graph"
                    } else {
                        "Expand details"
                    },
                )
                .ghost()
                .on_click(cx.listener(|pane, _, _, cx| {
                    pane.expanded = !pane.expanded;
                    cx.notify();
                })),
            );
        let working = if changes == 0 {
            "Working changes".to_owned()
        } else {
            format!("Working changes · {changes}")
        };
        let graph_col = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .child(self.search.clone())
            .child(
                div().px(px(SPACE_3)).py(px(SPACE_1)).child(
                    Button::new("history-changes", working)
                        .icon(IconName::Changes)
                        .ghost()
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(HistoryUiEvent::Changes))),
                ),
            )
            .child(div().flex_1().min_h(px(0.)).child(self.graph.clone()));

        if self.expanded {
            return div()
                .flex()
                .flex_col()
                .size_full()
                .child(header)
                .child(div().flex_1().min_h(px(0.)).child(self.detail.clone()))
                .into_any_element();
        }

        let resize_detail = cx.entity();
        let panes = div()
            .flex()
            .flex_1()
            .min_h(px(0.))
            .child(graph_col)
            .child(ResizeHandle::new(
                "history-detail-resize",
                self.detail_w,
                DETAIL_W,
                DETAIL_MIN,
                DETAIL_MAX,
                ResizeEdge::Left,
                move |width, _, cx| {
                    resize_detail.update(cx, |pane, cx| {
                        pane.set_detail_width(width, cx);
                        cx.emit(HistoryUiEvent::LayoutChanged);
                    });
                },
            ))
            .child(
                div()
                    .flex_none()
                    .w(px(self.detail_w))
                    .max_w(gpui::relative(0.55))
                    .child(self.detail.clone()),
            )
            .children(menu);
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .child(panes)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_shows_its_date_relative_to_now() {
        let meta = CommitMeta {
            subject: "Fix it".into(),
            author: "Ada".into(),
            seconds: 1_000_000_000 - 12 * 60,
            offset: 0,
        };
        let row = row_meta(&meta, 1_000_000_000);
        assert_eq!(row.subject.as_ref(), "Fix it");
        assert_eq!(row.author.as_ref(), "Ada");
        assert_eq!(row.date.as_ref(), "12 min ago");
    }

    #[test]
    fn labels_by_hex_match_the_ids_the_graph_prints() {
        // The graph looks labels up by `id.to_string()`: lower-case hex.
        let id = CommitId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        assert_eq!(id.to_string(), "0123456789abcdef0123456789abcdef01234567");
    }
}
