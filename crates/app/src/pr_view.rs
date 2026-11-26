//! Pull Requests (GitHub) and Merge Requests (GitLab / self-hosted) panel (P9):
//! - Lists PRs/MRs for the active repo's forge remote (with Open/Closed/All filter,
//!   search, and remote selector when multiple remotes exist).
//! - Shows PR/MR detail, review/approval status, "Open in browser", and
//!   "Checkout locally" (dispatches `Event::CheckoutPrRequested` to `RepoSession`,
//!   which enforces Law 9, Law 11, dirty-worktree refusal, and expected-ref guard).
//! - Provides a Create PR/MR form supporting both same-repo and cross-repo (fork)
//!   workflows, reachable from this panel or directly from the pre-push review
//!   screen after pushing.
//! - Forge service is opened lazily so core git features never touch the keychain
//!   or network until the user opens this panel.

use gpui::{
    AppContext, Context, ElementId, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Window, div, px,
};
use invoke_core::forge::{
    BranchUpstream, ConfiguredRemote, CreatePrProblem, ForgeKind, PrCreateTarget, PrFilter,
    PrListTarget, PrState, PrTargetError, PullRequest, default_pr_branch, filter_prs, pr_fetch_ref,
    resolve_create_target_with_config, resolve_remote_for_list, validate_create_pr,
};
use invoke_core::{BranchName, Event, Freshness, Head, LockState, Phase, Snapshot};
use shell::forge::{ForgeError, ForgeService};
use shell::read::{read_branch_upstream, read_remotes};
use tokens::{
    DIALOG, RADIUS_LG, RADIUS_MD, RADIUS_SM, SIDEBAR, SPACE_1, SPACE_2, SPACE_3, SPACE_4, SPACE_6,
    TEXT_BODY, TEXT_BODY_MEDIUM, TEXT_CODE_SMALL, TEXT_LABEL, TEXT_SMALL, TEXT_TITLE,
};
use ui::convert::{hsla, shadows};
use ui::focus;
use ui::{
    Banner, BannerKind, Button, CheckState, Checkbox, Divider, EditorEvent, Pill, PillTone,
    ResizeEdge, ResizeHandle, Segmented, TextEditor, TextStyleExt, ThemeExt,
};

use crate::forge_accounts::{AccountsEvent, AccountsView, is_web_url, open_service};
use crate::session::RepoSession;

/// Width of the PR/MR list column.
const PR_LIST_W: f32 = SIDEBAR + SPACE_4 * 7.0;
const PR_LIST_MIN: f32 = SIDEBAR;
const PR_LIST_MAX: f32 = DIALOG + SPACE_4 * 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Browse,
    Create,
    Accounts,
}

enum LoadPhase {
    Idle,
    Loading,
    Unavailable(String),
    NeedAccount { message: String },
    Failed { message: String, reconnect: bool },
    Ready(Box<ReadyState>),
}

struct ReadyState {
    target: PrListTarget,
    prs: Vec<PullRequest>,
    more: Option<String>,
    loading_more: bool,
    page_error: Option<String>,
    selected: Option<u64>,
    detail: Detail,
}

enum Detail {
    Idle,
    Loading,
    Ready(Box<PullRequest>),
    Failed(String),
}

enum CreateState {
    Idle,
    Submitting,
    Created(Box<PullRequest>),
    Failed { message: String, reconnect: bool },
}

pub struct PrPane {
    session: Entity<RepoSession>,
    service: Option<ForgeService>,
    accounts_view: Option<Entity<AccountsView>>,
    mode: Mode,
    /// Which screen to return to when closing Accounts (Browse or Create).
    return_mode: Mode,
    remotes: Vec<ConfiguredRemote>,
    /// Configured `(local_branch, branch.<name>.remote + merge)` for the
    /// branch it was read for, read off the UI thread.
    configured_upstream: Option<(String, BranchUpstream)>,
    preferred_remote: Option<String>,
    filter: PrFilter,
    search: Entity<TextEditor>,
    phase: LoadPhase,
    active: bool,
    /// Bumped on every list reload so stale background responses are dropped.
    list_gen: u64,
    /// Bumped on every detail load so stale detail responses are dropped.
    detail_gen: u64,
    /// Bumped on every create submission so stale responses never clobber newer state.
    create_gen: u64,
    // Create PR/MR form state
    create_base_remote: Option<String>,
    create_title: Entity<TextEditor>,
    create_base: Entity<TextEditor>,
    create_body: Entity<TextEditor>,
    create_draft: bool,
    create_state: CreateState,
    list_w: f32,
    _subscriptions: Vec<Subscription>,
}

impl PrPane {
    pub fn new(session: Entity<RepoSession>, cx: &mut Context<Self>) -> Self {
        let search =
            cx.new(|cx| TextEditor::new("Search title, author or branch", 1, cx).single_line());
        let create_title = cx.new(|cx| TextEditor::new("Pull request title", 1, cx).single_line());
        let create_base = cx.new(|cx| TextEditor::new("main", 1, cx).single_line());
        let create_body = cx.new(|cx| TextEditor::new("Describe your changes (optional)", 5, cx));
        let search_sub = cx.subscribe(&search, |this, _, event: &EditorEvent, cx| {
            if matches!(event, EditorEvent::Changed) {
                this.clear_hidden_selection(cx);
                cx.notify();
            }
        });
        let session_sub = cx.observe(&session, |this, _, cx| {
            if this.active {
                cx.notify();
            }
        });
        Self {
            session,
            service: None,
            accounts_view: None,
            mode: Mode::Browse,
            return_mode: Mode::Browse,
            remotes: Vec::new(),
            configured_upstream: None,
            preferred_remote: None,
            filter: PrFilter::Open,
            search,
            phase: LoadPhase::Idle,
            active: false,
            list_gen: 0,
            detail_gen: 0,
            create_gen: 0,
            create_base_remote: None,
            create_title,
            create_base,
            create_body,
            create_draft: false,
            create_state: CreateState::Idle,
            list_w: PR_LIST_W,
            _subscriptions: vec![search_sub, session_sub],
        }
    }

    pub fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == Mode::Browse && matches!(self.phase, LoadPhase::Ready(_)) {
            self.search
                .update(cx, |search, cx| search.focus(window, cx));
        }
    }

    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.mode = Mode::Browse;
        self.ensure_service_and_reload(cx);
    }

    pub fn open_create(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.mode = Mode::Create;
        // Preserve an in-flight submission so reopening Create never permits
        // a duplicate POST or drops the pending result.
        if !matches!(self.create_state, CreateState::Submitting) {
            self.create_state = CreateState::Idle;
        }
        self.prefill_create_form(cx);
        self.ensure_service_and_reload(cx);
    }

    pub fn close(&mut self) {
        self.active = false;
        self.list_gen += 1;
        self.detail_gen += 1;
    }

    fn open_accounts(&mut self, cx: &mut Context<Self>) {
        if self.mode != Mode::Accounts {
            self.return_mode = self.mode;
        }
        if let Some(service) = self.service.clone() {
            self.show_accounts_view(service, cx);
            return;
        }
        self.mode = Mode::Accounts;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_executor()
                .spawn(async move { open_service() })
                .await;
            this.update(cx, |this, cx| match opened {
                Ok(service) => {
                    this.service = Some(service.clone());
                    this.show_accounts_view(service, cx);
                }
                Err(error) => {
                    this.mode = this.return_mode;
                    this.phase = LoadPhase::Unavailable(error.user_message());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn show_accounts_view(&mut self, service: ForgeService, cx: &mut Context<Self>) {
        let view = cx.new(|cx| AccountsView::new(service, cx));
        let sub = cx.subscribe(&view, |this, _, event: &AccountsEvent, cx| match event {
            AccountsEvent::Changed => {
                this.ensure_service_and_reload(cx);
            }
            AccountsEvent::Closed => {
                this.mode = this.return_mode;
                this.accounts_view = None;
                this.ensure_service_and_reload(cx);
            }
        });
        self._subscriptions.push(sub);
        self.accounts_view = Some(view);
        self.mode = Mode::Accounts;
        cx.notify();
    }

    fn ensure_service_and_reload(&mut self, cx: &mut Context<Self>) {
        self.list_gen += 1;
        self.detail_gen += 1;
        let generation = self.list_gen;
        let existing_service = self.service.clone();
        let (paths, current_branch) = {
            let session = self.session.read(cx);
            let branch = session.state.snapshot.disk().and_then(|d| match &d.head {
                Head::Branch { name, .. } => Some(name.0.clone()),
                _ => None,
            });
            (session.discovered.paths.clone(), branch)
        };
        let preferred = self.preferred_remote.clone();
        let filter = self.filter;
        let selection = match &self.phase {
            LoadPhase::Ready(ready) => ready.selected.map(|id| (ready.target.clone(), id)),
            _ => None,
        };
        self.phase = LoadPhase::Loading;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let loaded =
                cx.background_executor()
                    .spawn(async move {
                        let service = match existing_service {
                            Some(s) => s,
                            None => open_service()
                                .map_err(|e| LoadErr::Unavailable(e.user_message()))?,
                        };
                        service.reload();
                        let remotes = read_remotes(&paths).map_err(LoadErr::Unavailable)?;
                        let configured_upstream = current_branch.as_deref().and_then(|b| {
                            read_branch_upstream(&paths, b).map(|u| (b.to_owned(), u))
                        });
                        let book = service.book();
                        let target = resolve_remote_for_list(&remotes, preferred.as_deref(), &book)
                            .map_err(|e| {
                                LoadErr::Target(
                                    service.clone(),
                                    remotes.clone(),
                                    configured_upstream.clone(),
                                    e,
                                )
                            })?;
                        let listing = service
                            .list_prs(&target.account.key(), &target.repo.path, filter, 1)
                            .map_err(|e| {
                                LoadErr::Api(
                                    service.clone(),
                                    remotes.clone(),
                                    configured_upstream.clone(),
                                    e,
                                )
                            })?;
                        Ok::<_, LoadErr>((service, remotes, configured_upstream, target, listing))
                    })
                    .await;
            this.update(cx, |this, cx| {
                if this.list_gen != generation {
                    return;
                }
                match loaded {
                    Ok((service, remotes, configured_upstream, target, listing)) => {
                        this.service = Some(service);
                        this.remotes = remotes;
                        this.configured_upstream = configured_upstream;
                        let visible = filter_prs(&listing.prs, this.search.read(cx).text());
                        let selected = selection
                            .filter(|(previous, id)| {
                                *previous == target
                                    && visible.iter().any(|&i| listing.prs[i].number == *id)
                            })
                            .map(|(_, id)| id)
                            .or_else(|| visible.first().map(|&i| listing.prs[i].number));
                        this.phase = LoadPhase::Ready(Box::new(ReadyState {
                            target,
                            prs: listing.prs,
                            more: listing.more,
                            loading_more: false,
                            page_error: None,
                            selected,
                            detail: Detail::Idle,
                        }));
                        if let Some(num) = selected {
                            this.load_detail(num, cx);
                        }
                    }
                    Err(LoadErr::Unavailable(msg)) => {
                        this.phase = LoadPhase::Unavailable(msg);
                    }
                    Err(LoadErr::Target(service, remotes, configured_upstream, err)) => {
                        this.service = Some(service);
                        this.remotes = remotes;
                        this.configured_upstream = configured_upstream;
                        match err {
                            PrTargetError::NoAccount { .. }
                            | PrTargetError::ChooseAccount { .. } => {
                                this.phase = LoadPhase::NeedAccount {
                                    message: target_error_text(&err),
                                };
                            }
                            other => {
                                this.phase = LoadPhase::Unavailable(target_error_text(&other));
                            }
                        }
                    }
                    Err(LoadErr::Api(service, remotes, configured_upstream, err)) => {
                        this.service = Some(service);
                        this.remotes = remotes;
                        this.configured_upstream = configured_upstream;
                        this.phase = LoadPhase::Failed {
                            reconnect: err.needs_reconnect(),
                            message: err.user_message(),
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_detail(&mut self, number: u64, cx: &mut Context<Self>) {
        let Some(service) = self.service.clone() else {
            return;
        };
        let (key, repo_path) = match &mut self.phase {
            LoadPhase::Ready(ready) => {
                if ready.selected == Some(number)
                    && matches!(ready.detail, Detail::Loading | Detail::Ready(_))
                {
                    return;
                }
                ready.selected = Some(number);
                ready.detail = Detail::Loading;
                (ready.target.account.key(), ready.target.repo.path.clone())
            }
            _ => return,
        };
        self.detail_gen += 1;
        let generation = self.detail_gen;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { service.get_pr_detail(&key, &repo_path, number) })
                .await;
            this.update(cx, |this, cx| {
                if this.detail_gen != generation {
                    return;
                }
                if let LoadPhase::Ready(ready) = &mut this.phase {
                    ready.detail = match result {
                        Ok(pr) => Detail::Ready(Box::new(pr)),
                        Err(error) => Detail::Failed(error.user_message()),
                    };
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn clear_hidden_selection(&mut self, cx: &Context<Self>) {
        if let LoadPhase::Ready(ready) = &mut self.phase
            && let Some(number) = ready.selected
            && !filter_prs(&ready.prs, self.search.read(cx).text())
                .iter()
                .any(|&i| ready.prs[i].number == number)
        {
            self.detail_gen += 1;
            ready.selected = None;
            ready.detail = Detail::Idle;
        }
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        let Some(service) = self.service.clone() else {
            return;
        };
        let (key, repo_path, cursor) = match &mut self.phase {
            LoadPhase::Ready(ready) => {
                if ready.loading_more {
                    return;
                }
                let Some(cursor) = ready.more.clone() else {
                    return;
                };
                ready.loading_more = true;
                ready.page_error = None;
                (
                    ready.target.account.key(),
                    ready.target.repo.path.clone(),
                    cursor,
                )
            }
            _ => return,
        };
        let filter = self.filter;
        let generation = self.list_gen;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { service.list_prs_page(&key, &repo_path, filter, Some(&cursor)) },
                )
                .await;
            this.update(cx, |this, cx| {
                if this.list_gen != generation {
                    return;
                }
                if let LoadPhase::Ready(ready) = &mut this.phase {
                    ready.loading_more = false;
                    match result {
                        Ok(page) => {
                            ready.prs.extend(page.prs);
                            ready.more = page.next;
                        }
                        Err(error) => ready.page_error = Some(error.user_message()),
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn checkout_pr(&mut self, number: u64, cx: &mut Context<Self>) {
        let LoadPhase::Ready(ready) = &self.phase else {
            return;
        };
        if ready.selected != Some(number) {
            return;
        }
        let target = &ready.target;
        let Some(remote_ref) = pr_fetch_ref(target.account.kind, number) else {
            return;
        };
        let Some(local_branch) = default_pr_branch(target.account.kind, number) else {
            return;
        };
        if self.checkout_blocker(&local_branch, cx).is_some() {
            return;
        }
        let event = Event::CheckoutPrRequested {
            remote: target.remote_name.clone(),
            remote_ref,
            local_branch: local_branch.0,
        };
        self.session
            .update(cx, |session, cx| session.deliver(event, cx));
    }

    fn checkout_blocker(&self, branch: &BranchName, cx: &Context<Self>) -> Option<String> {
        let state = &self.session.read(cx).state;
        let Some(disk) = state.snapshot.disk() else {
            return Some("Waiting for the repository to finish loading.".into());
        };
        if state.reads.freshness != Freshness::Fresh {
            return Some("Waiting for an up-to-date repository state.".into());
        }
        if state.ops.running.is_some()
            || !state.ops.queued.is_empty()
            || disk.phase != Phase::Clean
            || disk.lock != LockState::Free
        {
            return Some(
                "Finish the current Git operation before checking out this request.".into(),
            );
        }
        if !disk.status.entries.is_empty() {
            return Some(
                "Commit, stash, or discard your local changes before checking out.".into(),
            );
        }
        let target = branch.to_ref();
        if disk.refs.refs.contains_key(&target) {
            return Some(format!(
                "Local branch {} already exists. Switch to it from Branches.",
                branch.0
            ));
        }
        let target_prefix = format!("{}/", target.0);
        disk.refs.refs.keys().find_map(|reference| {
            let name = reference.0.strip_prefix("refs/heads/")?;
            (reference.0.starts_with(&target_prefix)
                || target
                    .0
                    .strip_prefix(reference.0.as_str())
                    .is_some_and(|rest| rest.starts_with('/')))
            .then(|| format!("Local branch {name} conflicts with the name {}.", branch.0))
        })
    }

    fn prefill_create_form(&mut self, cx: &mut Context<Self>) {
        let (default_title, default_base) = {
            let session = self.session.read(cx);
            let title = match session.state.snapshot.disk() {
                Some(disk) => match &disk.head {
                    Head::Branch { name, .. } => name.0.replace(['-', '_'], " "),
                    _ => String::new(),
                },
                None => String::new(),
            };
            let base = guess_default_base(&session.state.snapshot);
            (title, base)
        };
        if self.create_title.read(cx).text().trim().is_empty() && !default_title.is_empty() {
            self.create_title
                .update(cx, |ed, cx| ed.set_text(&default_title, cx));
        }
        if self.create_base.read(cx).text().trim().is_empty() {
            self.create_base
                .update(cx, |ed, cx| ed.set_text(default_base, cx));
        }
    }

    fn current_create_target(&self, cx: &Context<Self>) -> Result<PrCreateTarget, PrTargetError> {
        let Some(service) = &self.service else {
            return Err(PrTargetError::NoRemotes);
        };
        let Some(disk) = self.session.read(cx).state.snapshot.disk() else {
            return Err(PrTargetError::NotOnBranch);
        };
        let book = service.book();
        let current_branch = match &disk.head {
            Head::Branch { name, .. } => Some(name.0.as_str()),
            _ => None,
        };
        let matching_upstream = self
            .configured_upstream
            .as_ref()
            .filter(|(branch, _)| current_branch == Some(branch.as_str()))
            .map(|(_, upstream)| upstream);
        resolve_create_target_with_config(
            &self.remotes,
            disk,
            matching_upstream,
            self.create_base_remote.as_deref(),
            &book,
        )
    }

    fn submit_create(&mut self, cx: &mut Context<Self>) {
        if matches!(self.create_state, CreateState::Submitting) {
            return;
        }
        let Some(service) = self.service.clone() else {
            return;
        };
        let target = match self.current_create_target(cx) {
            Ok(t) => t,
            Err(err) => {
                self.create_state = CreateState::Failed {
                    message: target_error_text(&err),
                    reconnect: matches!(
                        err,
                        PrTargetError::NoAccount { .. } | PrTargetError::ChooseAccount { .. }
                    ),
                };
                cx.notify();
                return;
            }
        };
        let title = self.create_title.read(cx).text().to_owned();
        let base = self.create_base.read(cx).text().to_owned();
        let body = self.create_body.read(cx).text().to_owned();
        let input = match validate_create_pr(&title, &body, &base, self.create_draft, &target) {
            Ok(i) => i,
            Err(problem) => {
                self.create_state = CreateState::Failed {
                    message: create_problem_text(&problem).into(),
                    reconnect: false,
                };
                cx.notify();
                return;
            }
        };
        self.create_gen += 1;
        let generation = self.create_gen;
        let kind = target.account.kind;
        self.create_state = CreateState::Submitting;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { service.create_pr(&target, &input) })
                .await;
            this.update(cx, |this, cx| {
                if this.create_gen != generation {
                    return;
                }
                match result {
                    Ok(pr) => {
                        this.create_title.update(cx, |ed, cx| ed.set_text("", cx));
                        this.create_body.update(cx, |ed, cx| ed.set_text("", cx));
                        this.create_draft = false;
                        this.create_state = CreateState::Created(Box::new(pr));
                        this.ensure_service_and_reload(cx);
                    }
                    Err(err) => {
                        let (message, reconnect) = match (kind, &err) {
                            (ForgeKind::GitLab, ForgeError::Forbidden) => (
                                "GitLab refused merge request creation (HTTP 403). If this account was connected with `read_api`, reconnect it so it has the `api` scope."
                                    .into(),
                                true,
                            ),
                            (ForgeKind::GitHub, ForgeError::Forbidden) => (
                                "GitHub refused pull request creation (HTTP 403). Ensure your token has `repo` scope (classic) or `Pull requests: Read and write` access to this repository (fine-grained)."
                                    .into(),
                                true,
                            ),
                            _ => (err.user_message(), err.needs_reconnect()),
                        };
                        this.create_state = CreateState::Failed { message, reconnect };
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

enum LoadErr {
    Unavailable(String),
    Target(
        ForgeService,
        Vec<ConfiguredRemote>,
        Option<(String, BranchUpstream)>,
        PrTargetError,
    ),
    Api(
        ForgeService,
        Vec<ConfiguredRemote>,
        Option<(String, BranchUpstream)>,
        ForgeError,
    ),
}

fn guess_default_base(snapshot: &Snapshot) -> &'static str {
    if let Snapshot::Read { disk, .. } = snapshot {
        let has_main = disk
            .refs
            .refs
            .keys()
            .any(|r| r.0 == "refs/heads/main" || r.0.ends_with("/main"));
        if has_main {
            return "main";
        }
        let has_master = disk
            .refs
            .refs
            .keys()
            .any(|r| r.0 == "refs/heads/master" || r.0.ends_with("/master"));
        if has_master {
            return "master";
        }
    }
    "main"
}

pub fn target_error_text(err: &PrTargetError) -> String {
    match err {
        PrTargetError::NoRemotes => "This repository has no remotes configured.".into(),
        PrTargetError::RemoteNotFound { remote } => {
            format!("Remote '{remote}' is not configured in this repository.")
        }
        PrTargetError::NotOnForge { remote } => {
            format!("Remote '{remote}' points to a local path rather than a forge server.")
        }
        PrTargetError::NoAccount { remote, host } => format!(
            "Remote '{remote}' is on {host}, which has no connected forge account yet."
        ),
        PrTargetError::ChooseAccount { remote, .. } => format!(
            "Multiple forge accounts match remote '{remote}'. Choose the preferred account in Accounts."
        ),
        PrTargetError::InvalidRepoPath { remote } => format!(
            "Could not determine owner/repository from remote '{remote}'."
        ),
        PrTargetError::NotOnBranch => {
            "Check out a branch first. Pull/merge requests cannot be created from a detached or unborn HEAD."
                .into()
        }
        PrTargetError::NoUpstream { branch } => format!(
            "Branch '{branch}' has no upstream remote branch yet. Push it first before creating a pull/merge request."
        ),
        PrTargetError::UpstreamGone { branch } => format!(
            "Branch '{branch}' tracks an upstream branch that no longer exists on the remote. Push it first."
        ),
        PrTargetError::CrossHostMismatch => {
            "The selected base remote is on a different forge server than your branch's remote."
                .into()
        }
    }
}

pub fn create_problem_text(problem: &CreatePrProblem) -> &'static str {
    match problem {
        CreatePrProblem::EmptyTitle => "Enter a title for the pull/merge request.",
        CreatePrProblem::EmptyBaseBranch => "Enter a target base branch (for example `main`).",
        CreatePrProblem::SameBranch => {
            "The base branch cannot be the same as the current branch in the same repository."
        }
    }
}

fn kind_noun(kind: ForgeKind) -> &'static str {
    match kind {
        ForgeKind::GitHub => "Pull Request",
        ForgeKind::GitLab => "Merge Request",
    }
}

fn kind_prefix(kind: ForgeKind) -> &'static str {
    match kind {
        ForgeKind::GitHub => "#",
        ForgeKind::GitLab => "!",
    }
}

fn state_pill(state: PrState) -> Pill {
    let tone = match state {
        PrState::Open => PillTone::Success,
        PrState::Merged => PillTone::Accent,
        PrState::Closed => PillTone::Neutral,
    };
    Pill::new(state.label()).tone(tone)
}

fn source_branch(pr: &PullRequest, target: &PrListTarget) -> String {
    match &pr.head_repo {
        Some(repo) if !repo.eq_ignore_ascii_case(&target.repo.path) => {
            format!("{repo}:{}", pr.head_branch)
        }
        _ => pr.head_branch.clone(),
    }
}

impl Render for PrPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();

        if self.mode == Mode::Accounts
            && let Some(accounts) = &self.accounts_view
        {
            // AccountsView already renders its own centered dialog, padding,
            // and background (same pattern as CloneView).
            return div()
                .flex()
                .flex_1()
                .min_h(px(0.))
                .min_w(px(0.))
                .child(accounts.clone())
                .into_any_element();
        }

        if self.mode == Mode::Create {
            return self.render_create(cx).into_any_element();
        }

        // Top toolbar: Filter segmented control, remote switcher, Refresh, Accounts, + New PR
        let this = cx.entity();
        let filter_idx = match self.filter {
            PrFilter::Open => 0,
            PrFilter::Closed => 1,
            PrFilter::All => 2,
        };
        let filter_switch = Segmented::new(
            "pr-filter",
            vec!["Open".into(), "Closed".into(), "All".into()],
            filter_idx,
            move |idx, _, cx| {
                this.update(cx, |pane, cx| {
                    let filter = match idx {
                        0 => PrFilter::Open,
                        1 => PrFilter::Closed,
                        _ => PrFilter::All,
                    };
                    if pane.filter != filter {
                        pane.filter = filter;
                        pane.ensure_service_and_reload(cx);
                    }
                })
            },
        );

        let refresh_entity = cx.entity();
        let accounts_entity = cx.entity();
        let create_entity = cx.entity();
        let clear_search = self.search.clone();
        let mut toolbar = div()
            .flex()
            .flex_wrap()
            .flex_none()
            .items_center()
            .gap(px(SPACE_2))
            .px(px(SPACE_4))
            .py(px(SPACE_2))
            .bg(hsla(p.bg_panel))
            .border_b_1()
            .border_color(hsla(p.line))
            .child(filter_switch)
            .child(
                div()
                    .flex_1()
                    .min_w(px(SIDEBAR - SPACE_4 * 2.0))
                    .max_w(px(SIDEBAR))
                    .child(self.search.clone()),
            )
            .children((!self.search.read(cx).is_empty()).then(|| {
                Button::new("pr-clear-search", "Clear search")
                    .ghost()
                    .on_click(move |_, window, cx| {
                        clear_search.update(cx, |editor, cx| {
                            editor.set_text("", cx);
                            editor.focus(window, cx);
                        })
                    })
            }));

        if self.remotes.len() > 1 {
            let labels: Vec<SharedString> =
                self.remotes.iter().map(|r| r.name.clone().into()).collect();
            let active_remote = match &self.phase {
                LoadPhase::Ready(ready) => self
                    .remotes
                    .iter()
                    .position(|r| r.name == ready.target.remote_name)
                    .unwrap_or(0),
                _ => self
                    .preferred_remote
                    .as_deref()
                    .and_then(|name| self.remotes.iter().position(|r| r.name == name))
                    .unwrap_or(0),
            };
            let remote_names: Vec<String> = self.remotes.iter().map(|r| r.name.clone()).collect();
            let rem_entity = cx.entity();
            toolbar = toolbar.child(Segmented::new(
                "pr-remote",
                labels,
                active_remote,
                move |idx, _, cx| {
                    if let Some(name) = remote_names.get(idx).cloned() {
                        rem_entity.update(cx, |pane, cx| {
                            pane.preferred_remote = Some(name);
                            pane.ensure_service_and_reload(cx);
                        });
                    }
                },
            ));
        }

        let new_label = match &self.phase {
            LoadPhase::Ready(ready) => format!("New {}", kind_noun(ready.target.account.kind)),
            _ => "New PR / MR".into(),
        };
        toolbar = toolbar
            .child(div().flex_1())
            .child(
                Button::new("pr-refresh", "Refresh").on_click(move |_, _, cx| {
                    refresh_entity.update(cx, |pane, cx| pane.ensure_service_and_reload(cx))
                }),
            )
            .child(
                Button::new("pr-accounts", "Accounts")
                    .ghost()
                    .on_click(move |_, _, cx| {
                        accounts_entity.update(cx, |pane, cx| pane.open_accounts(cx))
                    }),
            )
            .child(
                Button::new("pr-new", new_label)
                    .primary()
                    .on_click(move |_, _, cx| {
                        create_entity.update(cx, |pane, cx| pane.open_create(cx))
                    }),
            );

        let content = match &self.phase {
            LoadPhase::Idle | LoadPhase::Loading => self
                .centered_message("Loading pull requests…", None, cx)
                .into_any_element(),
            LoadPhase::Unavailable(msg) => self.centered_message(msg, None, cx).into_any_element(),
            LoadPhase::NeedAccount { message } => {
                let acc = cx.entity();
                let btn = Button::new("pr-connect-account", "Connect account…")
                    .primary()
                    .on_click(move |_, _, cx| acc.update(cx, |pane, cx| pane.open_accounts(cx)))
                    .into_any_element();
                self.centered_message(message, Some(btn), cx)
                    .into_any_element()
            }
            LoadPhase::Failed { message, reconnect } => {
                let retry = cx.entity();
                let acc = cx.entity();
                let mut actions = div().flex().gap(px(SPACE_2)).child(
                    Button::new("pr-retry", "Try again").on_click(move |_, _, cx| {
                        retry.update(cx, |pane, cx| pane.ensure_service_and_reload(cx))
                    }),
                );
                if *reconnect {
                    actions = actions.child(
                        Button::new("pr-reconnect", "Reconnect…")
                            .primary()
                            .on_click(move |_, _, cx| {
                                acc.update(cx, |pane, cx| pane.open_accounts(cx))
                            }),
                    );
                }
                self.centered_message(message, Some(actions.into_any_element()), cx)
                    .into_any_element()
            }
            LoadPhase::Ready(ready) => self.render_ready(ready, window, cx).into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .bg(hsla(p.bg_app))
            .child(toolbar)
            .child(content)
            .into_any_element()
    }
}

impl PrPane {
    fn centered_message(
        &self,
        text: &str,
        extra: Option<gpui::AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = cx.palette();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .items_center()
            .justify_center()
            .gap(px(SPACE_3))
            .p(px(SPACE_4))
            .text_style(&TEXT_BODY, cx)
            .text_color(hsla(p.ink_muted))
            .child(text.to_owned())
            .children(extra)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_ready(
        &self,
        ready: &ReadyState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let p = cx.palette();
        let ring = focus::ring_color(cx);
        let target = &ready.target;
        let prs = &ready.prs;
        let query = self.search.read(cx).text();
        let searching = !query.trim().is_empty();
        let visible = filter_prs(prs, query);
        let prefix = kind_prefix(target.account.kind);
        let noun = kind_noun(target.account.kind);

        let mut list = div()
            .id("pr-list-scroll")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(self.list_w))
            .h_full()
            .overflow_y_scroll()
            .gap(px(SPACE_1))
            .p(px(SPACE_2))
            .bg(hsla(p.bg_panel))
            .child(
                div()
                    .px(px(SPACE_2))
                    .py(px(SPACE_1))
                    .text_style(&TEXT_LABEL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(format!(
                        "{} · {} of {} loaded",
                        target.repo.path,
                        visible.len(),
                        prs.len()
                    )),
            );

        if visible.is_empty() {
            list = list.child(
                div()
                    .px(px(SPACE_2))
                    .py(px(SPACE_3))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.ink_muted))
                    .child(if searching && ready.more.is_some() {
                        "No matches in the loaded requests. Load more to continue searching."
                            .to_owned()
                    } else if searching {
                        "No matching requests.".to_owned()
                    } else {
                        match self.filter {
                            PrFilter::Open => "No open requests.",
                            PrFilter::Closed => "No closed requests.",
                            PrFilter::All => "No requests.",
                        }
                        .to_owned()
                    }),
            );
        }

        for idx in visible {
            let pr = &prs[idx];
            let is_sel = ready.selected == Some(pr.number);
            let num = pr.number;
            let sel_entity = cx.entity();
            let hover = hsla(p.bg_hover);
            let row_id = ElementId::from(("pr-row", idx));
            let handle = focus::handle(&row_id, window, cx);
            let mut row = div()
                .id(row_id)
                .track_focus(&handle)
                .tab_stop(true)
                .relative()
                .flex()
                .flex_col()
                .gap(px(SPACE_1))
                .px(px(SPACE_2))
                .py(px(SPACE_2))
                .rounded(px(RADIUS_MD))
                .cursor_pointer()
                .focus_visible(move |style| focus::ring(style, ring))
                .on_click(move |_, _, cx| {
                    sel_entity.update(cx, |pane, cx| pane.load_detail(num, cx))
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(SPACE_2))
                        .child(
                            div()
                                .flex_none()
                                .text_style(&TEXT_CODE_SMALL, cx)
                                .text_color(hsla(p.ink_muted))
                                .child(format!("{prefix}{}", pr.number)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .text_style(&TEXT_BODY_MEDIUM, cx)
                                .text_color(hsla(p.ink))
                                .child(pr.title.clone()),
                        )
                        .child(state_pill(pr.state).compact()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(SPACE_2))
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .truncate()
                                .child(pr.author.clone()),
                        )
                        .children(
                            pr.draft
                                .then(|| Pill::new("Draft").tone(PillTone::Neutral).compact()),
                        ),
                )
                .child(
                    div()
                        .truncate()
                        .text_style(&TEXT_CODE_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(format!(
                            "{} → {}",
                            source_branch(pr, target),
                            pr.base_branch
                        )),
                );
            row = if is_sel {
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
                row.hover(move |s| s.bg(hover))
            };
            list = list.child(row);
        }

        if let Some(error) = &ready.page_error {
            list = list.child(
                div()
                    .px(px(SPACE_2))
                    .text_style(&TEXT_SMALL, cx)
                    .text_color(hsla(p.warning))
                    .child(error.clone()),
            );
        }
        if ready.more.is_some() {
            let more_entity = cx.entity();
            list = list.child(
                div().p(px(SPACE_2)).child(
                    Button::new(
                        "pr-load-more",
                        if ready.loading_more {
                            "Loading…"
                        } else {
                            "Load more"
                        },
                    )
                    .disabled(ready.loading_more)
                    .on_click(move |_, _, cx| {
                        more_entity.update(cx, |pane, cx| pane.load_more(cx))
                    }),
                ),
            );
        }

        // Right detail pane
        let shown_pr = match &ready.detail {
            Detail::Ready(pr) => Some(pr.as_ref()),
            _ => ready
                .selected
                .and_then(|num| prs.iter().find(|pr| pr.number == num)),
        };

        let detail_pane = match shown_pr {
            None => self
                .centered_message(
                    &format!("Select a {} to inspect it.", noun.to_lowercase()),
                    None,
                    cx,
                )
                .into_any_element(),
            Some(pr) => {
                let number = pr.number;
                let checkout_entity = cx.entity();
                let branch = default_pr_branch(target.account.kind, number);
                let checkout_blocker = branch
                    .as_ref()
                    .and_then(|branch| self.checkout_blocker(branch, cx));
                let web_url = pr.web_url.clone();
                let can_open = is_web_url(&web_url);

                let review_label = pr.review_status.label();
                let review_tone = match review_label {
                    "Approved" => PillTone::Success,
                    "Changes requested" => PillTone::Warning,
                    _ => PillTone::Neutral,
                };
                let review_pill = Pill::new(review_label).tone(review_tone);
                let mut meta = div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(SPACE_2))
                    .child(state_pill(pr.state))
                    .children(pr.draft.then(|| Pill::new("Draft").tone(PillTone::Neutral)))
                    .children(matches!(ready.detail, Detail::Ready(_)).then_some(review_pill));
                if let Some(count) = pr.comments_count {
                    meta = meta.child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(format!("{count} comments")),
                    );
                }
                if let Some(mergeable) = pr.mergeable {
                    meta = meta.child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(if mergeable { p.add_ink } else { p.warning }))
                            .child(if mergeable {
                                "Mergeable"
                            } else {
                                "Has conflicts"
                            }),
                    );
                }

                let mut actions = div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(SPACE_2))
                    .children(branch.map(|branch| {
                        Button::new("pr-checkout", format!("Checkout as {}", branch.0))
                            .primary()
                            .disabled(checkout_blocker.is_some())
                            .on_click(move |_, _, cx| {
                                checkout_entity.update(cx, |pane, cx| pane.checkout_pr(number, cx))
                            })
                    }));
                if can_open {
                    actions = actions.child(
                        Button::new("pr-open-web", "Open in browser").on_click(move |_, _, cx| {
                            cx.open_url(&web_url);
                        }),
                    );
                }
                let detail_status = match &ready.detail {
                    Detail::Loading => Some(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child("Loading review status…"),
                    ),
                    Detail::Failed(error) => {
                        let retry = cx.entity();
                        Some(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(px(SPACE_2))
                                .text_style(&TEXT_SMALL, cx)
                                .text_color(hsla(p.warning))
                                .child(format!("Could not load request details. {error}"))
                                .child(
                                    Button::new("pr-detail-retry", "Retry details")
                                        .compact()
                                        .on_click(move |_, _, cx| {
                                            retry
                                                .update(cx, |pane, cx| pane.load_detail(number, cx))
                                        }),
                                ),
                        )
                    }
                    _ => None,
                };

                div()
                    .id("pr-detail-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .overflow_y_scroll()
                    .bg(hsla(p.bg_panel))
                    .gap(px(SPACE_3))
                    .p(px(SPACE_4))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(SPACE_1))
                            .child(
                                div()
                                    .text_style(&TEXT_TITLE, cx)
                                    .text_color(hsla(p.ink))
                                    .child(format!("{prefix}{}  {}", pr.number, pr.title)),
                            )
                            .child(
                                div()
                                    .text_style(&TEXT_SMALL, cx)
                                    .text_color(hsla(p.ink_muted))
                                    .child(format!("Opened by {}", pr.author)),
                            ),
                    )
                    .child(meta)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(SPACE_1))
                            .text_style(&TEXT_CODE_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(format!("Source · {}", source_branch(pr, target)))
                            .child(format!("Target · {}:{}", target.repo.path, pr.base_branch)),
                    )
                    .child(actions)
                    .children(checkout_blocker.map(|message| {
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child(message)
                    }))
                    .children(detail_status)
                    .child(Divider::horizontal())
                    .child(
                        div()
                            .text_style(&TEXT_BODY, cx)
                            .text_color(hsla(if pr.body.trim().is_empty() {
                                p.ink_muted
                            } else {
                                p.ink
                            }))
                            .child(if pr.body.trim().is_empty() {
                                "No description provided.".to_owned()
                            } else {
                                pr.body.clone()
                            }),
                    )
                    .into_any_element()
            }
        };

        let resize_list = cx.entity();
        div()
            .flex()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .child(list)
            .child(ResizeHandle::new(
                "pr-list-resize",
                self.list_w,
                PR_LIST_W,
                PR_LIST_MIN,
                PR_LIST_MAX,
                ResizeEdge::Right,
                move |width, _, cx| {
                    resize_list.update(cx, |pane, cx| {
                        pane.list_w = width;
                        cx.notify();
                    });
                },
            ))
            .child(detail_pane)
    }

    fn render_create(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let target = self.current_create_target(cx);
        let back_entity = cx.entity();
        let submit_entity = cx.entity();
        let draft_entity = cx.entity();
        let acc_entity = cx.entity();

        let noun = match &target {
            Ok(t) => kind_noun(t.account.kind),
            Err(_) => "Pull / Merge Request",
        };

        let mut card = div()
            .flex()
            .flex_col()
            .w(px(DIALOG))
            .gap(px(SPACE_3))
            .p(px(SPACE_6))
            .bg(hsla(p.bg_panel))
            .border_1()
            .border_color(hsla(p.line))
            .rounded(px(RADIUS_LG))
            .shadow(shadows(p.shadow_overlay))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_style(&TEXT_TITLE, cx)
                            .text_color(hsla(p.ink))
                            .child(format!("Create {noun}")),
                    )
                    .child(
                        Button::new("pr-create-back", "Back to list")
                            .ghost()
                            .on_click(move |_, _, cx| {
                                back_entity.update(cx, |pane, cx| {
                                    pane.mode = Mode::Browse;
                                    cx.notify();
                                })
                            }),
                    ),
            );

        match &target {
            Ok(t) => {
                let route = if t.is_fork {
                    format!(
                        "{}:{} -> {} ({})",
                        t.head_repo.path, t.head_branch, t.base_repo.path, t.base_remote
                    )
                } else {
                    format!(
                        "{} -> {} ({})",
                        t.head_branch, t.base_repo.path, t.base_remote
                    )
                };
                card = card.child(
                    div()
                        .text_style(&TEXT_SMALL, cx)
                        .text_color(hsla(p.ink_muted))
                        .child(format!("Source: {route} · Account: {}", t.account.label())),
                );
            }
            Err(err) => {
                let mut banner = Banner::new(BannerKind::Warning, target_error_text(err));
                if matches!(
                    err,
                    PrTargetError::NoAccount { .. } | PrTargetError::ChooseAccount { .. }
                ) {
                    let acc_btn = acc_entity.clone();
                    banner = banner.action(Button::new("pr-create-connect", "Accounts…").on_click(
                        move |_, _, cx| acc_btn.update(cx, |pane, cx| pane.open_accounts(cx)),
                    ));
                }
                card = card.child(banner);
            }
        }

        // Always keep the target remote selector visible when multiple remotes
        // exist so the user can recover from a CrossHostMismatch selection.
        if self.remotes.len() > 1 {
            let labels: Vec<SharedString> =
                self.remotes.iter().map(|r| r.name.clone().into()).collect();
            let active_name = match &target {
                Ok(t) => Some(t.base_remote.as_str()),
                Err(_) => self.create_base_remote.as_deref(),
            };
            let active = active_name
                .and_then(|name| self.remotes.iter().position(|r| r.name == name))
                .unwrap_or(0);
            let names: Vec<String> = self.remotes.iter().map(|r| r.name.clone()).collect();
            let rem_entity = cx.entity();
            card = card.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(SPACE_2))
                    .child(
                        div()
                            .text_style(&TEXT_SMALL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child("Target remote:"),
                    )
                    .child(Segmented::new(
                        "pr-create-remote",
                        labels,
                        active,
                        move |idx, _, cx| {
                            if let Some(name) = names.get(idx).cloned() {
                                rem_entity.update(cx, |pane, cx| {
                                    pane.create_base_remote = Some(name);
                                    cx.notify();
                                });
                            }
                        },
                    )),
            );
        }

        if let CreateState::Failed { message, reconnect } = &self.create_state {
            let mut banner = Banner::new(BannerKind::Error, message.clone());
            if *reconnect {
                let acc_btn = acc_entity.clone();
                banner = banner.action(Button::new("pr-create-reconnect", "Reconnect…").on_click(
                    move |_, _, cx| acc_btn.update(cx, |pane, cx| pane.open_accounts(cx)),
                ));
            }
            card = card.child(banner);
        }
        if let CreateState::Created(pr) = &self.create_state {
            let url = pr.web_url.clone();
            let mut banner = Banner::new(
                BannerKind::Success,
                format!("Created #{}: {}", pr.number, pr.title),
            );
            if is_web_url(&url) {
                banner = banner.action(
                    Button::new("pr-created-open", "Open in browser")
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                );
            }
            card = card.child(banner);
        }

        let submitting = matches!(self.create_state, CreateState::Submitting);
        let can_submit = target.is_ok() && !submitting;

        card = card
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_1))
                    .child(
                        div()
                            .text_style(&TEXT_LABEL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child("TITLE"),
                    )
                    .child(self.create_title.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_1))
                    .child(
                        div()
                            .text_style(&TEXT_LABEL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child("BASE BRANCH"),
                    )
                    .child(self.create_base.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(SPACE_1))
                    .child(
                        div()
                            .text_style(&TEXT_LABEL, cx)
                            .text_color(hsla(p.ink_muted))
                            .child("DESCRIPTION"),
                    )
                    .child(self.create_body.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        Checkbox::new(
                            "pr-create-draft",
                            if self.create_draft {
                                CheckState::Checked
                            } else {
                                CheckState::Unchecked
                            },
                        )
                        .label("Create as draft")
                        .on_click(move |_, _, cx| {
                            draft_entity.update(cx, |pane, cx| {
                                pane.create_draft = !pane.create_draft;
                                cx.notify();
                            })
                        }),
                    )
                    .child(
                        Button::new(
                            "pr-create-submit",
                            if submitting {
                                "Creating…".to_owned()
                            } else {
                                format!("Create {noun}")
                            },
                        )
                        .primary()
                        .disabled(!can_submit)
                        .on_click(move |_, _, cx| {
                            submit_entity.update(cx, |pane, cx| pane.submit_create(cx))
                        }),
                    ),
            );

        div()
            .id("pr-create-scroll")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .items_center()
            .overflow_y_scroll()
            .bg(hsla(p.bg_app))
            .p(px(SPACE_4))
            .child(card)
    }
}
