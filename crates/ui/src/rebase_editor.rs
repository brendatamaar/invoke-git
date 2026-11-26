//! Interactive rebase editor component (P4b): a reorderable list of commits
//! with drag-to-reorder, up/down controls, per-commit action picker
//! (`pick`, `squash`, `fixup`, `edit`, `drop`), and live plan validation.
//!
//! All colours, typography, spacing, radii and row heights come from `tokens`.
//! The model (`RebaseEditorModel`) is pure and tested independently of GPUI.

use gpui::{
    Context, CursorStyle, ElementId, EventEmitter, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use invoke_core::{Oid, PlanError, RebaseAction, RebasePlan};
use tokens::{
    RADIUS_MD, RADIUS_SM, ROW_LIST, SPACE_1, SPACE_2, SPACE_3, SPACE_4, SPACE_6, SPACE_8,
    TEXT_BODY, TEXT_BODY_MEDIUM, TEXT_CODE_SMALL, TEXT_SMALL, TEXT_TITLE,
};

use crate::components::{Banner, BannerKind, Button, Pill, PillTone};
use crate::convert::hsla;
use crate::focus;
use crate::text::TextStyleExt;
use crate::theme::ThemeExt;

/// All supported interactive rebase actions in menu order.
pub const ALL_ACTIONS: [RebaseAction; 5] = [
    RebaseAction::Pick,
    RebaseAction::Squash,
    RebaseAction::Fixup,
    RebaseAction::Edit,
    RebaseAction::Drop,
];

/// Short label for an action badge or dropdown item.
pub const fn action_label(action: RebaseAction) -> &'static str {
    match action {
        RebaseAction::Pick => "pick",
        RebaseAction::Squash => "squash",
        RebaseAction::Fixup => "fixup",
        RebaseAction::Edit => "edit",
        RebaseAction::Drop => "drop",
    }
}

/// One-line explanation shown in the action dropdown.
pub const fn action_description(action: RebaseAction) -> &'static str {
    match action {
        RebaseAction::Pick => "Keep this commit as it is",
        RebaseAction::Squash => "Meld into the previous kept commit",
        RebaseAction::Fixup => "Meld into previous and discard message",
        RebaseAction::Edit => "Pause after applying so you can amend",
        RebaseAction::Drop => "Remove this commit from history",
    }
}

/// Plain-English explanation of a [`PlanError`].
pub const fn plan_error_message(error: PlanError) -> &'static str {
    match error {
        PlanError::Empty => "There are no commits to rebase.",
        PlanError::ContainsUpstream => "The upstream commit cannot be part of the rebase plan.",
        PlanError::DuplicateCommit => "A commit appears more than once in the plan.",
        PlanError::MissingSquashTarget => {
            "The first kept commit cannot be squashed or fixed up. Change it to pick or edit, or move another kept commit above it."
        }
    }
}

/// One commit row in the interactive rebase editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseRow {
    pub oid: Oid,
    pub short_hash: SharedString,
    pub subject: SharedString,
    pub author: SharedString,
    pub action: RebaseAction,
}

impl RebaseRow {
    pub fn new(
        oid: Oid,
        subject: impl Into<SharedString>,
        author: impl Into<SharedString>,
    ) -> Self {
        let hex = oid.as_hex();
        let short = hex[..7.min(hex.len())].to_owned();
        Self {
            oid,
            short_hash: short.into(),
            subject: subject.into(),
            author: author.into(),
            action: RebaseAction::Pick,
        }
    }
}

/// Summary counts of actions currently selected in the plan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActionCounts {
    pub pick: usize,
    pub squash: usize,
    pub fixup: usize,
    pub edit: usize,
    pub drop: usize,
}

/// Pure, testable model of the interactive rebase plan being edited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseEditorModel {
    upstream: Oid,
    rows: Vec<RebaseRow>,
}

impl RebaseEditorModel {
    pub fn new(upstream: Oid, rows: Vec<RebaseRow>) -> Self {
        Self { upstream, rows }
    }

    pub fn upstream(&self) -> &Oid {
        &self.upstream
    }

    pub fn rows(&self) -> &[RebaseRow] {
        &self.rows
    }

    /// Move row at `from` to index `to`. Returns `true` when the order changed.
    pub fn move_row(&mut self, from: usize, to: usize) -> bool {
        if from >= self.rows.len() || to >= self.rows.len() || from == to {
            return false;
        }
        let row = self.rows.remove(from);
        self.rows.insert(to, row);
        true
    }

    /// Move row at `index` one slot earlier (older).
    pub fn move_up(&mut self, index: usize) -> bool {
        if index == 0 || index >= self.rows.len() {
            return false;
        }
        self.move_row(index, index - 1)
    }

    /// Move row at `index` one slot later (newer).
    pub fn move_down(&mut self, index: usize) -> bool {
        if index + 1 >= self.rows.len() {
            return false;
        }
        self.move_row(index, index + 1)
    }

    /// Set the action for row `index`. Returns `true` when it changed.
    pub fn set_action(&mut self, index: usize, action: RebaseAction) -> bool {
        let Some(row) = self.rows.get_mut(index) else {
            return false;
        };
        if row.action == action {
            return false;
        }
        row.action = action;
        true
    }

    /// Whether `index` has a kept (non-`Drop`) commit before it to squash/fixup into.
    pub fn can_squash_or_fixup(&self, index: usize) -> bool {
        if index >= self.rows.len() {
            return false;
        }
        self.rows[..index]
            .iter()
            .any(|row| row.action != RebaseAction::Drop)
    }

    /// Row-level warning when this specific row causes a validation error.
    pub fn row_warning(&self, index: usize) -> Option<&'static str> {
        let row = self.rows.get(index)?;
        if matches!(row.action, RebaseAction::Squash | RebaseAction::Fixup)
            && !self.can_squash_or_fixup(index)
        {
            return Some("Needs a kept commit above");
        }
        None
    }

    /// Validate and build a core [`RebasePlan`].
    pub fn to_plan(&self) -> Result<RebasePlan, PlanError> {
        let steps = self
            .rows
            .iter()
            .map(|row| (row.oid.clone(), row.action))
            .collect();
        RebasePlan::new(self.upstream.clone(), steps)
    }

    /// Current plan validation error, if any.
    pub fn validation_error(&self) -> Option<PlanError> {
        self.to_plan().err()
    }

    /// Counts of each action in the current plan.
    pub fn counts(&self) -> ActionCounts {
        let mut counts = ActionCounts::default();
        for row in &self.rows {
            match row.action {
                RebaseAction::Pick => counts.pick += 1,
                RebaseAction::Squash => counts.squash += 1,
                RebaseAction::Fixup => counts.fixup += 1,
                RebaseAction::Edit => counts.edit += 1,
                RebaseAction::Drop => counts.drop += 1,
            }
        }
        counts
    }
}

/// Events emitted by [`RebaseEditor`] to its host view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseEditorEvent {
    /// Emitted whenever rows are reordered or an action changes.
    PlanChanged(Result<RebasePlan, PlanError>),
    /// The user clicked Start rebase.
    StartRequested,
    /// The user clicked Cancel.
    CancelRequested,
}

/// GPUI interactive rebase plan editor.
pub struct RebaseEditor {
    model: RebaseEditorModel,
    upstream_label: SharedString,
    /// Row index whose action picker menu is currently open.
    open_menu: Option<usize>,
    /// Row index currently being dragged with the primary mouse button.
    dragging: Option<usize>,
    /// Row index currently hovered as the drop target while dragging.
    drag_target: Option<usize>,
    /// External guard (e.g., busy or uncommitted changes) disabling Start.
    start_disabled_reason: Option<SharedString>,
}

impl EventEmitter<RebaseEditorEvent> for RebaseEditor {}

impl RebaseEditor {
    pub fn new(model: RebaseEditorModel, upstream_label: impl Into<SharedString>) -> Self {
        Self {
            model,
            upstream_label: upstream_label.into(),
            open_menu: None,
            dragging: None,
            drag_target: None,
            start_disabled_reason: None,
        }
    }

    pub fn model(&self) -> &RebaseEditorModel {
        &self.model
    }

    pub fn set_start_disabled_reason(
        &mut self,
        reason: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if self.start_disabled_reason != reason {
            self.start_disabled_reason = reason;
            cx.notify();
        }
    }

    fn emit_change(&mut self, cx: &mut Context<Self>) {
        cx.emit(RebaseEditorEvent::PlanChanged(self.model.to_plan()));
        cx.notify();
    }

    fn move_up(&mut self, index: usize, cx: &mut Context<Self>) {
        self.open_menu = None;
        if self.model.move_up(index) {
            self.emit_change(cx);
        }
    }

    fn move_down(&mut self, index: usize, cx: &mut Context<Self>) {
        self.open_menu = None;
        if self.model.move_down(index) {
            self.emit_change(cx);
        }
    }

    fn set_action(&mut self, index: usize, action: RebaseAction, cx: &mut Context<Self>) {
        self.open_menu = None;
        if self.model.set_action(index, action) {
            self.emit_change(cx);
        } else {
            cx.notify();
        }
    }

    fn toggle_menu(&mut self, index: usize, cx: &mut Context<Self>) {
        self.open_menu = if self.open_menu == Some(index) {
            None
        } else {
            Some(index)
        };
        cx.notify();
    }

    fn start_drag(&mut self, index: usize, cx: &mut Context<Self>) {
        self.open_menu = None;
        self.dragging = Some(index);
        self.drag_target = Some(index);
        cx.notify();
    }

    fn update_drag(&mut self, target: usize, cx: &mut Context<Self>) {
        if self.dragging.is_some() && self.drag_target != Some(target) {
            self.drag_target = Some(target);
            cx.notify();
        }
    }

    fn finish_drag(&mut self, cx: &mut Context<Self>) {
        let Some(from) = self.dragging.take() else {
            return;
        };
        let to = self.drag_target.take().unwrap_or(from);
        if self.model.move_row(from, to) {
            self.emit_change(cx);
        } else {
            cx.notify();
        }
    }

    fn cancel_drag(&mut self, cx: &mut Context<Self>) {
        if self.dragging.take().is_some() || self.drag_target.take().is_some() {
            cx.notify();
        }
    }
}

fn action_colors(action: RebaseAction, p: &tokens::Palette) -> (tokens::Rgba, tokens::Rgba) {
    match action {
        RebaseAction::Pick => (p.bg_raised, p.ink),
        RebaseAction::Squash | RebaseAction::Fixup => (p.accent_soft, p.accent),
        RebaseAction::Edit => (p.warning_soft, p.warning),
        RebaseAction::Drop => (p.remove_bg, p.remove_ink),
    }
}

fn format_counts(counts: ActionCounts) -> String {
    let mut parts = Vec::new();
    if counts.pick > 0 {
        parts.push(format!("{} pick", counts.pick));
    }
    if counts.squash > 0 {
        parts.push(format!("{} squash", counts.squash));
    }
    if counts.fixup > 0 {
        parts.push(format!("{} fixup", counts.fixup));
    }
    if counts.edit > 0 {
        parts.push(format!("{} edit", counts.edit));
    }
    if counts.drop > 0 {
        parts.push(format!("{} drop", counts.drop));
    }
    if parts.is_empty() {
        "No commits".into()
    } else {
        parts.join(" · ")
    }
}

impl Render for RebaseEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let validation = self.model.validation_error();
        let counts = self.model.counts();
        let row_count = self.model.rows().len();
        let can_start = validation.is_none() && self.start_disabled_reason.is_none();

        let (cancel, start) = (cx.entity(), cx.entity());
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(px(SPACE_3))
            .px(px(SPACE_4))
            .py(px(SPACE_3))
            .bg(hsla(p.bg_panel))
            .border_b_1()
            .border_color(hsla(p.line))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w(px(0.))
                    .gap(px(SPACE_1))
                    .child(
                        div()
                            .text_style(&TEXT_TITLE, cx)
                            .text_color(hsla(p.ink))
                            .child(format!("Interactive rebase onto {}", self.upstream_label)),
                    )
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(format!(
                                "Oldest first · drag rows or use arrows to reorder · {}",
                                format_counts(counts)
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(SPACE_2))
                    .child(Button::new("rebase-cancel", "Cancel").ghost().on_click(
                        move |_, _, cx| {
                            cancel.update(cx, |_, cx| cx.emit(RebaseEditorEvent::CancelRequested));
                        },
                    ))
                    .child(
                        Button::new("rebase-start", "Start rebase")
                            .primary()
                            .disabled(!can_start)
                            .on_click(move |_, _, cx| {
                                start
                                    .update(cx, |_, cx| cx.emit(RebaseEditorEvent::StartRequested));
                            }),
                    ),
            );

        let mut banners = div().flex().flex_col().flex_none().gap(px(SPACE_2));
        let mut has_banner = false;
        if let Some(error) = validation {
            banners = banners.child(Banner::new(BannerKind::Warning, plan_error_message(error)));
            has_banner = true;
        }
        if let Some(reason) = &self.start_disabled_reason {
            banners = banners.child(Banner::new(BannerKind::Warning, reason.clone()));
            has_banner = true;
        }
        let banner_strip = has_banner.then(|| {
            banners
                .px(px(SPACE_4))
                .py(px(SPACE_2))
                .bg(hsla(p.bg_panel))
                .border_b_1()
                .border_color(hsla(p.line))
        });

        let mut list = div()
            .id("rebase-row-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .p(px(SPACE_4))
            .gap(px(SPACE_2))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.finish_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.cancel_drag(cx)),
            );

        for (index, row) in self.model.rows().iter().enumerate() {
            let is_dragged = self.dragging == Some(index);
            let is_drop_target = self.dragging.is_some()
                && self.drag_target == Some(index)
                && self.dragging != Some(index);
            let drop_above = self.dragging.is_some_and(|from| index < from);
            let row_warn = self.model.row_warning(index);
            let (badge_bg, badge_fg) = action_colors(row.action, p);
            let is_drop = row.action == RebaseAction::Drop;
            let menu_open = self.open_menu == Some(index);

            let (up_ent, down_ent, menu_ent) = (cx.entity(), cx.entity(), cx.entity());

            let drag_id = ElementId::from(("rebase-drag", index));
            let drag_focus = focus::handle(&drag_id, window, cx);
            let handle = div()
                .id(drag_id)
                .track_focus(&drag_focus)
                .tab_stop(true)
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .w(px(SPACE_6))
                .h(px(ROW_LIST))
                .rounded(px(RADIUS_SM))
                .cursor(if is_dragged {
                    CursorStyle::ClosedHand
                } else {
                    CursorStyle::OpenHand
                })
                .text_style(&TEXT_BODY_MEDIUM, cx)
                .text_color(hsla(p.ink_faint))
                .hover(|style| style.bg(hsla(p.bg_hover)).text_color(hsla(p.ink)))
                .focus_visible(move |style| focus::ring(style, ring))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| this.start_drag(index, cx)),
                )
                .child("⋮⋮");

            let order_badge = div()
                .flex_none()
                .w(px(SPACE_6))
                .text_right()
                .text_style(&TEXT_CODE_SMALL, cx)
                .text_color(hsla(p.ink_faint))
                .child(format!("{}", index + 1));

            let action_id = ElementId::from(("rebase-action", index));
            let action_focus = focus::handle(&action_id, window, cx);
            let action_btn = div()
                .id(action_id)
                .track_focus(&action_focus)
                .tab_stop(true)
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .gap(px(SPACE_1))
                .h(px(ROW_LIST))
                .px(px(SPACE_2))
                .rounded(px(RADIUS_SM))
                .bg(hsla(badge_bg))
                .border_1()
                .border_color(hsla(if row_warn.is_some() {
                    p.warning
                } else {
                    p.line_control
                }))
                .cursor_pointer()
                .hover(|style| style.opacity(0.9))
                .focus_visible(move |style| focus::ring(style, ring))
                .text_style(&TEXT_BODY_MEDIUM, cx)
                .text_color(hsla(badge_fg))
                .on_click(move |_, _, cx| {
                    menu_ent.update(cx, |this, cx| this.toggle_menu(index, cx));
                })
                .child(action_label(row.action))
                .child("▾");

            let hash = div()
                .flex_none()
                .text_style(&TEXT_CODE_SMALL, cx)
                .text_color(hsla(p.ink_muted))
                .child(row.short_hash.clone());

            let subject = div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_style(&TEXT_BODY, cx)
                .text_color(hsla(if is_drop { p.ink_muted } else { p.ink }))
                .child(row.subject.clone());

            let author = div()
                .flex_none()
                .max_w(px(SPACE_8 * 4.))
                .truncate()
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink_faint))
                .child(row.author.clone());

            let warn_pill = row_warn.map(|msg| Pill::new(msg).tone(PillTone::Warning).compact());

            let arrows = div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(SPACE_1))
                .child(
                    Button::new(("rebase-up", index), "↑")
                        .ghost()
                        .disabled(index == 0)
                        .on_click(move |_, _, cx| {
                            up_ent.update(cx, |this, cx| this.move_up(index, cx));
                        }),
                )
                .child(
                    Button::new(("rebase-down", index), "↓")
                        .ghost()
                        .disabled(index + 1 >= row_count)
                        .on_click(move |_, _, cx| {
                            down_ent.update(cx, |this, cx| this.move_down(index, cx));
                        }),
                );

            let mut row_card = div()
                .id(("rebase-row", index))
                .flex()
                .flex_col()
                .rounded(px(RADIUS_MD))
                .border_1()
                .border_color(hsla(if is_drop_target {
                    p.accent
                } else if row_warn.is_some() {
                    p.warning
                } else {
                    p.line
                }))
                .bg(hsla(if is_dragged {
                    p.accent_soft
                } else {
                    p.bg_panel
                }))
                .on_mouse_move(cx.listener(move |this, _, _, cx| {
                    this.update_drag(index, cx);
                }));

            if is_drop_target {
                row_card = if drop_above {
                    row_card.border_t_2()
                } else {
                    row_card.border_b_2()
                };
            }

            let main_line = div()
                .flex()
                .items_center()
                .gap(px(SPACE_2))
                .px(px(SPACE_2))
                .py(px(SPACE_2))
                .child(handle)
                .child(order_badge)
                .child(action_btn)
                .child(hash)
                .child(subject)
                .children(warn_pill)
                .child(author)
                .child(arrows);

            row_card = row_card.child(main_line);

            if menu_open {
                let mut menu = div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(SPACE_2))
                    .px(px(SPACE_3))
                    .py(px(SPACE_2))
                    .bg(hsla(p.bg_raised))
                    .border_t_1()
                    .border_color(hsla(p.line));
                for action in ALL_ACTIONS {
                    let chosen = row.action == action;
                    let (abg, afg) = action_colors(action, p);
                    let pick_ent = cx.entity();
                    let pick_id =
                        ElementId::from(("rebase-pick-action", index * 8 + action as usize));
                    let pick_focus = focus::handle(&pick_id, window, cx);
                    let hover_bg = hsla(p.bg_hover);
                    let mut pick_item = div()
                        .id(pick_id)
                        .track_focus(&pick_focus)
                        .tab_stop(true)
                        .flex()
                        .items_center()
                        .gap(px(SPACE_1))
                        .px(px(SPACE_2))
                        .py(px(SPACE_1))
                        .rounded(px(RADIUS_SM))
                        .border_1()
                        .border_color(hsla(if chosen { p.accent } else { p.line_control }))
                        .bg(hsla(abg))
                        .cursor_pointer()
                        .focus_visible(move |style| focus::ring(style, ring))
                        .on_click(move |_, _, cx| {
                            pick_ent.update(cx, |this, cx| {
                                this.set_action(index, action, cx);
                            });
                        })
                        .child(
                            div()
                                .text_style(&TEXT_BODY_MEDIUM, cx)
                                .text_color(hsla(afg))
                                .child(action_label(action)),
                        )
                        .child(
                            div()
                                .text_style(&TEXT_SMALL, cx)
                                .text_color(hsla(p.ink_muted))
                                .child(action_description(action)),
                        );
                    pick_item = if chosen {
                        pick_item.hover(|style| style.opacity(0.9))
                    } else {
                        pick_item.hover(move |style| style.bg(hover_bg))
                    };
                    menu = menu.child(pick_item);
                }
                row_card = row_card.child(menu);
            }

            list = list.child(row_card);
        }

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(hsla(p.bg_app))
            .child(header)
            .children(banner_strip)
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;

    fn oid(n: u8) -> Oid {
        Oid::from_hex(&format!("{n:040x}")).unwrap()
    }

    fn model_of(count: u8) -> RebaseEditorModel {
        let rows = (1..=count)
            .map(|n| RebaseRow::new(oid(n), format!("Commit {n}"), "Ada"))
            .collect();
        RebaseEditorModel::new(oid(0), rows)
    }

    fn arb_action() -> impl Strategy<Value = RebaseAction> {
        prop_oneof![
            Just(RebaseAction::Pick),
            Just(RebaseAction::Squash),
            Just(RebaseAction::Fixup),
            Just(RebaseAction::Edit),
            Just(RebaseAction::Drop),
        ]
    }

    #[test]
    fn first_kept_commit_cannot_squash_or_fixup() {
        let mut model = model_of(3);
        assert_eq!(model.validation_error(), None);
        assert!(!model.can_squash_or_fixup(0));
        assert!(model.can_squash_or_fixup(1));

        assert!(model.set_action(0, RebaseAction::Squash));
        assert_eq!(
            model.validation_error(),
            Some(PlanError::MissingSquashTarget)
        );
        assert!(model.row_warning(0).is_some());

        // Dropping row 0 makes row 1 the first kept commit.
        model.set_action(0, RebaseAction::Drop);
        model.set_action(1, RebaseAction::Fixup);
        assert!(!model.can_squash_or_fixup(1));
        assert_eq!(
            model.validation_error(),
            Some(PlanError::MissingSquashTarget)
        );
        assert!(model.row_warning(1).is_some());

        // Moving row 2 (Pick) to index 0 makes row 1 (now index 2) valid.
        assert!(model.move_row(2, 0));
        assert_eq!(model.validation_error(), None);
        assert_eq!(model.row_warning(2), None);
    }

    #[test]
    fn move_up_and_down_respect_bounds() {
        let mut model = model_of(2);
        assert!(!model.move_up(0));
        assert!(!model.move_down(1));
        assert!(model.move_down(0));
        assert_eq!(model.rows()[0].oid, oid(2));
        assert_eq!(model.rows()[1].oid, oid(1));
        assert!(model.move_up(1));
        assert_eq!(model.rows()[0].oid, oid(1));
    }

    #[test]
    fn counts_track_every_action() {
        let mut model = model_of(5);
        model.set_action(1, RebaseAction::Squash);
        model.set_action(2, RebaseAction::Fixup);
        model.set_action(3, RebaseAction::Edit);
        model.set_action(4, RebaseAction::Drop);
        assert_eq!(
            model.counts(),
            ActionCounts {
                pick: 1,
                squash: 1,
                fixup: 1,
                edit: 1,
                drop: 1,
            }
        );
    }

    proptest! {
        /// Any sequence of reorders preserves the exact set of commits without duplicates or loss.
        #[test]
        fn reorders_preserve_commit_set(
            count in 1u8..12,
            moves in prop::collection::vec((0usize..12, 0usize..12), 0..24),
        ) {
            let mut model = model_of(count);
            let original: BTreeSet<Oid> = model.rows().iter().map(|r| r.oid.clone()).collect();
            for (from, to) in moves {
                model.move_row(from, to);
            }
            let after: BTreeSet<Oid> = model.rows().iter().map(|r| r.oid.clone()).collect();
            prop_assert_eq!(model.rows().len(), usize::from(count));
            prop_assert_eq!(after, original);
        }

        /// UI validation agrees with core RebasePlan::new for every action assignment.
        #[test]
        fn validation_matches_first_kept_rule(
            actions in prop::collection::vec(arb_action(), 1..10),
        ) {
            let mut model = model_of(actions.len() as u8);
            for (i, action) in actions.iter().copied().enumerate() {
                model.set_action(i, action);
            }
            let first_kept = actions.iter().find(|a| **a != RebaseAction::Drop).copied();
            let should_fail = matches!(
                first_kept,
                Some(RebaseAction::Squash | RebaseAction::Fixup)
            );
            prop_assert_eq!(
                model.validation_error(),
                should_fail.then_some(PlanError::MissingSquashTarget)
            );
            let any_row_warn = (0..actions.len()).any(|i| model.row_warning(i).is_some());
            prop_assert_eq!(any_row_warn, should_fail);
        }
    }
}
