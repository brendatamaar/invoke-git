//! Interactive rebase page (P4b): loads a linear commit range from `shell`,
//! hosts `ui::RebaseEditor` for reordering and action selection, and shows
//! running/paused rebase controls (Continue, Skip, Abort) with integration
//! to Changes (for `edit` pauses) and Conflicts.

use gpui::{
    AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement, Render, SharedString,
    Styled, Subscription, Window, div, px,
};
use invoke_core::{
    DiskState, Event, FileChange, Head, Oid, Phase, RebasePause, RebaseState, RepoState, Snapshot,
};
use shell::rebase::read_rebase_commits;
use tokens::{
    RADIUS_MD, SPACE_2, SPACE_3, SPACE_4, TEXT_BODY, TEXT_BODY_MEDIUM, TEXT_SMALL, TEXT_TITLE,
};
use ui::convert::hsla;
use ui::{
    Banner, BannerKind, Button, RebaseEditor, RebaseEditorEvent, RebaseEditorModel, RebaseRow,
    TextStyleExt, ThemeExt,
};

use crate::session::RepoSession;

/// Navigation events emitted by [`RebaseView`] to `RepoView`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebaseViewEvent {
    BackToHistory,
    OpenChanges,
    OpenConflicts,
}

/// Extract the commit oid of `HEAD`, if it points to a commit.
pub fn head_oid(head: &Head) -> Option<&Oid> {
    match head {
        Head::Branch { oid, .. } | Head::Detached(oid) => Some(oid),
        Head::Unborn(_) => None,
    }
}

/// Whether `target` can be offered as an "Interactive rebase onto…" base.
pub fn rebase_target_allowed(disk: &DiskState, target: &Oid) -> bool {
    disk.phase == Phase::Clean && head_oid(&disk.head).is_some_and(|head| head != target)
}

/// Reason why "Start rebase" should be disabled right now, if any.
pub fn start_blocker(state: &RepoState) -> Option<&'static str> {
    if state.ops.running.is_some() {
        return Some("Wait for the current operation to finish.");
    }
    let Some(disk) = state.snapshot.disk() else {
        return Some("The repository has not been read yet.");
    };
    if disk.phase != Phase::Clean {
        return Some("Another Git operation is already in progress.");
    }
    let dirty_tracked = disk.status.entries.iter().any(|entry| {
        entry.staged.is_some()
            || entry
                .unstaged
                .as_ref()
                .is_some_and(|change| *change != FileChange::Untracked)
    });
    if dirty_tracked {
        return Some("Commit or stash your changes to tracked files before starting a rebase.");
    }
    None
}

/// Display name of the branch being rebased (or a short hash / fallback).
pub fn origin_label(disk: &DiskState) -> String {
    if let Phase::Rebasing {
        origin: Some(origin),
        ..
    } = &disk.phase
    {
        if let Some(branch) = &origin.branch {
            return branch.0.clone();
        }
        let hex = origin.tip.as_hex();
        return hex[..7.min(hex.len())].to_owned();
    }
    match &disk.head {
        Head::Branch { name, .. } => name.0.clone(),
        _ => "your branch".to_owned(),
    }
}

/// Heading for a paused rebase banner.
pub fn paused_heading(step: u32, total: u32, pause: RebasePause, branch: &str) -> String {
    match pause {
        RebasePause::Edit => {
            format!("Rebasing {branch}: paused for edit (step {step} of {total})")
        }
        RebasePause::Conflict => {
            format!("Rebasing {branch}: conflict at step {step} of {total}")
        }
        RebasePause::Other => {
            format!("Rebasing {branch}: paused at step {step} of {total}")
        }
    }
}

/// Secondary instruction text for a paused rebase banner.
pub fn paused_detail(pause: RebasePause, unresolved: usize) -> String {
    if unresolved > 0 {
        return format!(
            "{unresolved} conflicted {} must be resolved before continuing.",
            if unresolved == 1 { "file" } else { "files" }
        );
    }
    match pause {
        RebasePause::Edit => {
            "Edit or amend the commit in Changes if needed, then continue the rebase.".into()
        }
        RebasePause::Conflict | RebasePause::Other => {
            "Continue to apply the next commit, skip this commit, or abort to restore the branch."
                .into()
        }
    }
}

enum Stage {
    Idle,
    Loading(u64),
    Editing {
        editor: Entity<RebaseEditor>,
        _sub: Subscription,
    },
    LoadFailed(String),
}

pub struct RebaseView {
    session: Entity<RepoSession>,
    stage: Stage,
    confirm_abort: bool,
    request: u64,
    _observe: Subscription,
}

impl EventEmitter<RebaseViewEvent> for RebaseView {}

impl RebaseView {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&session, |this, _, cx| this.session_changed(cx));
        Self {
            session,
            stage: Stage::Idle,
            confirm_abort: false,
            request: 0,
            _observe: observe,
        }
    }

    /// Begin loading commits in `upstream..HEAD` for interactive rebase.
    pub fn open(&mut self, upstream: Oid, cx: &mut Context<Self>) {
        self.confirm_abort = false;
        let head = match &self.session.read(cx).state.snapshot {
            Snapshot::Read { disk, .. } => head_oid(&disk.head).cloned(),
            Snapshot::NotRead => None,
        };
        let Some(head) = head else {
            self.stage = Stage::LoadFailed("Interactive rebase needs an existing HEAD.".into());
            cx.notify();
            return;
        };
        self.request += 1;
        let request = self.request;
        self.stage = Stage::Loading(request);
        cx.notify();
        let paths = self.session.read(cx).discovered.paths.clone();
        cx.spawn(async move |this, cx| {
            let up = upstream.clone();
            let read = cx
                .background_executor()
                .spawn(async move { read_rebase_commits(&paths, &up, &head) })
                .await;
            this.update(cx, |this, cx| {
                if !matches!(this.stage, Stage::Loading(r) if r == request) {
                    return;
                }
                match read {
                    Ok(commits) => {
                        let rows: Vec<RebaseRow> = commits
                            .into_iter()
                            .map(|c| RebaseRow::new(c.oid, c.subject, c.author))
                            .collect();
                        let model = RebaseEditorModel::new(upstream.clone(), rows);
                        let initial_plan = model.to_plan();
                        let hex = upstream.as_hex();
                        let label = hex[..7.min(hex.len())].to_owned();
                        let editor = cx.new(|_| RebaseEditor::new(model, label));
                        let sub = cx.subscribe(&editor, |this, editor, event, cx| {
                            this.editor_event(&editor, event, cx)
                        });
                        this.stage = Stage::Editing { editor, _sub: sub };
                        if let Ok(plan) = initial_plan {
                            this.deliver(Event::RebasePlanEdited(plan), cx);
                        }
                        this.sync_editor_guard(cx);
                    }
                    Err(message) => {
                        this.stage = Stage::LoadFailed(message);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn editor_event(
        &mut self,
        editor: &Entity<RebaseEditor>,
        event: &RebaseEditorEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            RebaseEditorEvent::PlanChanged(Ok(plan)) => {
                self.deliver(Event::RebasePlanEdited(plan.clone()), cx);
            }
            RebaseEditorEvent::PlanChanged(Err(_)) => {}
            RebaseEditorEvent::StartRequested => {
                if let Ok(plan) = editor.read(cx).model().to_plan() {
                    self.deliver(Event::RebasePlanEdited(plan), cx);
                    self.deliver(Event::RebaseStartRequested, cx);
                }
            }
            RebaseEditorEvent::CancelRequested => {
                if matches!(
                    self.session.read(cx).state.rebase,
                    RebaseState::Editing { .. }
                ) {
                    self.deliver(Event::AbortRequested, cx);
                }
                self.stage = Stage::Idle;
                cx.emit(RebaseViewEvent::BackToHistory);
                cx.notify();
            }
        }
    }

    fn deliver(&mut self, event: Event, cx: &mut Context<Self>) {
        self.session
            .update(cx, |session, cx| session.deliver(event, cx));
    }

    fn sync_editor_guard(&self, cx: &mut Context<Self>) {
        if let Stage::Editing { editor, .. } = &self.stage {
            let reason = start_blocker(&self.session.read(cx).state).map(SharedString::from);
            editor.update(cx, |ed, cx| ed.set_start_disabled_reason(reason, cx));
        }
    }

    fn session_changed(&mut self, cx: &mut Context<Self>) {
        self.sync_editor_guard(cx);
        let state = &self.session.read(cx).state;
        if !matches!(
            state.snapshot.disk().map(|d| &d.phase),
            Some(Phase::Rebasing { .. })
        ) {
            self.confirm_abort = false;
        }
        if matches!(state.rebase, RebaseState::Done | RebaseState::Aborted)
            && matches!(self.stage, Stage::Editing { .. })
        {
            self.stage = Stage::Idle;
        }
        cx.notify();
    }
}

impl Render for RebaseView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let state = &self.session.read(cx).state;
        let busy = state.ops.running.is_some();

        // 1. Paused rebase on disk: show step progress and Continue / Skip / Abort.
        if let Some(disk) = state.snapshot.disk()
            && let Phase::Rebasing {
                step,
                total,
                conflicts,
                pause,
                ..
            } = &disk.phase
        {
            let branch = origin_label(disk);
            let unresolved = conflicts.unresolved_count();
            let heading = paused_heading(*step, *total, *pause, &branch);
            let detail = paused_detail(*pause, unresolved);
            let ready = matches!(
                invoke_core::continue_gate(&disk.phase),
                invoke_core::ContinueGate::Ready
            );

            let mut actions = div().flex().flex_wrap().items_center().gap(px(SPACE_2));
            if self.confirm_abort {
                let (yes, no) = (cx.entity(), cx.entity());
                actions = actions
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink))
                            .child(format!(
                                "Abort the rebase? {branch} will be restored to its original commit."
                            )),
                    )
                    .child(
                        Button::new("rebase-abort-yes", "Abort rebase")
                            .danger()
                            .disabled(busy)
                            .on_click(move |_, _, cx| {
                                yes.update(cx, |this, cx| {
                                    this.confirm_abort = false;
                                    this.deliver(Event::AbortRequested, cx);
                                })
                            }),
                    )
                    .child(
                        Button::new("rebase-abort-no", "Keep rebasing")
                            .ghost()
                            .on_click(move |_, _, cx| {
                                no.update(cx, |this, cx| {
                                    this.confirm_abort = false;
                                    cx.notify();
                                })
                            }),
                    );
            } else {
                let (open_changes, open_conflicts, skip, abort, cont) = (
                    cx.entity(),
                    cx.entity(),
                    cx.entity(),
                    cx.entity(),
                    cx.entity(),
                );
                if unresolved > 0 {
                    actions = actions.child(
                        Button::new("rebase-open-conflicts", "Open Conflicts")
                            .primary()
                            .on_click(move |_, _, cx| {
                                open_conflicts
                                    .update(cx, |_, cx| cx.emit(RebaseViewEvent::OpenConflicts));
                            }),
                    );
                } else {
                    actions =
                        actions.child(Button::new("rebase-open-changes", "Open Changes").on_click(
                            move |_, _, cx| {
                                open_changes
                                    .update(cx, |_, cx| cx.emit(RebaseViewEvent::OpenChanges));
                            },
                        ));
                }
                actions = actions
                    .child(
                        Button::new("rebase-skip", "Skip this commit")
                            .disabled(busy)
                            .on_click(move |_, _, cx| {
                                skip.update(cx, |this, cx| this.deliver(Event::SkipRequested, cx));
                            }),
                    )
                    .child(
                        Button::new("rebase-abort", "Abort rebase")
                            .disabled(busy)
                            .on_click(move |_, _, cx| {
                                abort.update(cx, |this, cx| {
                                    this.confirm_abort = true;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("rebase-continue", "Continue rebase")
                            .primary()
                            .disabled(!ready || busy)
                            .on_click(move |_, _, cx| {
                                cont.update(cx, |this, cx| {
                                    this.deliver(Event::ContinueRequested, cx)
                                });
                            }),
                    );
            }

            return div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.))
                .min_h(px(0.))
                .bg(hsla(p.bg_app))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_between()
                        .gap(px(SPACE_3))
                        .px(px(SPACE_4))
                        .py(px(SPACE_3))
                        .bg(hsla(p.warning_soft))
                        .border_b_1()
                        .border_color(hsla(p.line))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w(px(0.))
                                .child(
                                    div()
                                        .text_style(&TEXT_BODY_MEDIUM, cx)
                                        .text_color(hsla(p.warning))
                                        .child(heading),
                                )
                                .child(
                                    div()
                                        .text_style(&TEXT_SMALL, cx)
                                        .text_color(hsla(p.ink))
                                        .child(detail),
                                ),
                        )
                        .child(actions),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .p(px(SPACE_4))
                        .gap(px(SPACE_3))
                        .child(
                            div()
                                .p(px(SPACE_4))
                                .rounded(px(RADIUS_MD))
                                .bg(hsla(p.bg_panel))
                                .border_1()
                                .border_color(hsla(p.line))
                                .flex()
                                .flex_col()
                                .gap(px(SPACE_2))
                                .child(
                                    div()
                                        .text_style(&TEXT_TITLE, cx)
                                        .text_color(hsla(p.ink))
                                        .child(format!("Step {step} of {total}")),
                                )
                                .child(
                                    div()
                                        .text_style(&TEXT_BODY, cx)
                                        .text_color(hsla(p.ink_muted))
                                        .child(match pause {
                                            RebasePause::Edit => "Git paused at this commit because its action is edit. Open Changes to modify files and amend the commit, then click Continue rebase.",
                                            RebasePause::Conflict => "This commit has merge conflicts. Open Conflicts to resolve each file, then continue or skip the commit.",
                                            RebasePause::Other => "Git paused the rebase. Continue when ready, skip this commit, or abort to put the branch back as it was.",
                                        }),
                                ),
                        ),
                )
                .into_any_element();
        }

        // 2. Rebase is queued or actively running.
        if matches!(
            state.rebase,
            RebaseState::Queued(_) | RebaseState::Running { .. } | RebaseState::Reading { .. }
        ) {
            let abort = cx.entity();
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(SPACE_3))
                .p(px(SPACE_4))
                .bg(hsla(p.bg_app))
                .child(
                    div()
                        .text_style(&TEXT_TITLE, cx)
                        .text_color(hsla(p.ink))
                        .child("Running interactive rebase…"),
                )
                .child(
                    div()
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child("Applying your rebase plan in Git."),
                )
                .child(
                    Button::new("rebase-running-abort", "Abort rebase").on_click(
                        move |_, _, cx| {
                            abort.update(cx, |this, cx| this.deliver(Event::AbortRequested, cx));
                        },
                    ),
                )
                .into_any_element();
        }

        // 3. Plan editing / loading / error / idle.
        match &self.stage {
            Stage::Editing { editor, .. } => editor.clone().into_any_element(),
            Stage::Loading(_) => {
                let back = cx.entity();
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap(px(SPACE_3))
                    .p(px(SPACE_4))
                    .bg(hsla(p.bg_app))
                    .child(
                        div()
                            .text_style(&TEXT_BODY, cx)
                            .text_color(hsla(p.ink_muted))
                            .child("Reading commits for interactive rebase…"),
                    )
                    .child(
                        Button::new("rebase-loading-cancel", "Cancel")
                            .ghost()
                            .on_click(move |_, _, cx| {
                                back.update(cx, |this, cx| {
                                    this.stage = Stage::Idle;
                                    cx.emit(RebaseViewEvent::BackToHistory);
                                });
                            }),
                    )
                    .into_any_element()
            }
            Stage::LoadFailed(message) => {
                let back = cx.entity();
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .p(px(SPACE_4))
                    .gap(px(SPACE_3))
                    .bg(hsla(p.bg_app))
                    .child(Banner::new(BannerKind::Warning, message.clone()))
                    .child(div().flex().child(
                        Button::new("rebase-error-back", "Back to History").on_click(
                            move |_, _, cx| {
                                back.update(cx, |this, cx| {
                                    this.stage = Stage::Idle;
                                    cx.emit(RebaseViewEvent::BackToHistory);
                                });
                            },
                        ),
                    ))
                    .into_any_element()
            }
            Stage::Idle => {
                let back = cx.entity();
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap(px(SPACE_3))
                    .p(px(SPACE_4))
                    .bg(hsla(p.bg_app))
                    .child(
                        div()
                            .text_style(&TEXT_BODY, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(
                                "Select a commit in History and choose Interactive rebase onto…",
                            ),
                    )
                    .child(Button::new("rebase-idle-back", "Back to History").on_click(
                        move |_, _, cx| {
                            back.update(cx, |_, cx| cx.emit(RebaseViewEvent::BackToHistory));
                        },
                    ))
                    .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use invoke_core::{
        BranchName, Conflicts, IndexId, LockState, ReadId, RebaseOrigin, RefsSnapshot, RepoId,
        RepoPath, StatusEntry, StatusSnapshot,
    };
    use proptest::prelude::*;

    use super::*;

    fn oid(n: u8) -> Oid {
        Oid::from_hex(&format!("{n:040x}")).unwrap()
    }

    fn clean_disk(head: Head, status: StatusSnapshot) -> DiskState {
        DiskState {
            head,
            phase: Phase::Clean,
            index: IndexId::Absent,
            status,
            refs: RefsSnapshot::default(),
            stashes: Vec::new(),
            lock: LockState::Free,
            advanced: Default::default(),
        }
    }

    fn state_with(disk: DiskState) -> RepoState {
        let mut state = RepoState::new(RepoId {
            root_commit: None,
            remote_url: None,
        });
        state.snapshot = Snapshot::Read {
            disk,
            read: ReadId(1),
        };
        state
    }

    #[test]
    fn target_must_differ_from_head_and_require_clean_phase() {
        let head = Head::Branch {
            name: BranchName("main".into()),
            oid: oid(2),
        };
        let disk = clean_disk(head.clone(), StatusSnapshot::default());
        assert!(rebase_target_allowed(&disk, &oid(1)));
        assert!(!rebase_target_allowed(&disk, &oid(2)));

        let mut busy = clean_disk(head, StatusSnapshot::default());
        busy.phase = Phase::Merging {
            conflicts: Conflicts::default(),
        };
        assert!(!rebase_target_allowed(&busy, &oid(1)));
    }

    #[test]
    fn start_blocker_allows_untracked_files_but_blocks_tracked_edits() {
        let head = Head::Branch {
            name: BranchName("main".into()),
            oid: oid(2),
        };
        let untracked = StatusSnapshot::from_entries([StatusEntry {
            path: RepoPath("scratch.txt".into()),
            staged: None,
            unstaged: Some(FileChange::Untracked),
        }]);
        assert_eq!(
            start_blocker(&state_with(clean_disk(head.clone(), untracked))),
            None
        );

        let modified = StatusSnapshot::from_entries([StatusEntry {
            path: RepoPath("lib.rs".into()),
            staged: None,
            unstaged: Some(FileChange::Modified),
        }]);
        assert!(start_blocker(&state_with(clean_disk(head, modified))).is_some());
    }

    #[test]
    fn paused_heading_and_origin_label_name_branch_and_step() {
        let mut disk = clean_disk(Head::Detached(oid(3)), StatusSnapshot::default());
        disk.phase = Phase::Rebasing {
            step: 2,
            total: 5,
            conflicts: Conflicts::default(),
            pause: RebasePause::Edit,
            origin: Some(RebaseOrigin {
                branch: Some(BranchName("topic".into())),
                tip: oid(4),
            }),
        };
        let label = origin_label(&disk);
        assert_eq!(label, "topic");
        assert_eq!(
            paused_heading(2, 5, RebasePause::Edit, &label),
            "Rebasing topic: paused for edit (step 2 of 5)"
        );
    }

    proptest! {
        /// Paused heading always contains the step/total numbers and branch name.
        #[test]
        fn paused_heading_carries_step_total_and_branch(
            step in 1u32..500,
            total in 1u32..500,
            kind in 0u8..3,
        ) {
            let pause = match kind {
                0 => RebasePause::Edit,
                1 => RebasePause::Conflict,
                _ => RebasePause::Other,
            };
            let text = paused_heading(step, total, pause, "feature");
            let expected = format!("step {step} of {total}");
            prop_assert!(text.contains("feature"));
            prop_assert!(text.contains(&expected));
        }
    }
}
