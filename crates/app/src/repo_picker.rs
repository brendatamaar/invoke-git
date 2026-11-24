//! "Clone from account": pick one of the repositories a connected account
//! can see. Choosing one only fills in the clone dialog's address; the
//! clone itself is the dialog's, with git's own credentials.
//!
//! The list comes from the forge in the background (a few pages at a time),
//! is filtered as the user types, and says what went wrong in words: a token
//! the forge no longer accepts offers to reconnect.

use gpui::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, Subscription, Task, UniformListScrollHandle,
    Window, div, px, uniform_list,
};
use invoke_core::forge::{ForgeAccount, RemoteRepo, Visibility, filter_repos};
use shell::forge::{ForgeError, ForgeService};
use tokens::{RADIUS_SM, ROW_LIST, SPACE_2, SPACE_3, TEXT_BODY, TEXT_SMALL};
use ui::convert::hsla;
use ui::{Banner, BannerKind, Button, EditorEvent, Segmented, TextEditor, TextStyleExt, ThemeExt};

/// Pages fetched per request (100 repositories each).
const PAGES_PER_LOAD: usize = 5;
/// Rows the list is as tall as at most.
const VISIBLE_ROWS: usize = 8;

pub enum PickerEvent {
    Picked(RemoteRepo),
    /// The user wants the accounts screen.
    ManageAccounts,
}

/// What the picker knows about the selected account's repositories.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listing {
    NoAccount,
    Loading,
    Loaded {
        repos: Vec<RemoteRepo>,
        /// Where the next load would start; `None` when everything is here.
        more: Option<String>,
    },
    Failed(ForgeError),
}

impl Listing {
    pub fn repos(&self) -> &[RemoteRepo] {
        match self {
            Listing::Loaded { repos, .. } => repos,
            _ => &[],
        }
    }
}

/// The line under the filter: how many repositories show.
pub fn summary(listing: &Listing, shown: usize) -> String {
    match listing {
        Listing::NoAccount => String::new(),
        Listing::Loading => "Loading repositories…".into(),
        Listing::Failed(_) => String::new(),
        Listing::Loaded { repos, more } => {
            let total = repos.len();
            let tail = if more.is_some() {
                " (more available)"
            } else {
                ""
            };
            if shown == total {
                format!("{total} repositories{tail}")
            } else {
                format!("{shown} of {total} repositories{tail}")
            }
        }
    }
}

pub fn visibility_label(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "Public",
        Visibility::Private => "Private",
        Visibility::Internal => "Internal",
    }
}

pub struct RepoPicker {
    service: ForgeService,
    accounts: Vec<ForgeAccount>,
    selected: usize,
    listing: Listing,
    /// Bumped on every load so an answer for a stale request is dropped.
    generation: u64,
    filter: Entity<TextEditor>,
    scroll: UniformListScrollHandle,
    task: Option<Task<()>>,
    _subscription: Subscription,
}

impl EventEmitter<PickerEvent> for RepoPicker {}

impl RepoPicker {
    pub fn new(service: ForgeService, cx: &mut Context<Self>) -> Self {
        let filter =
            cx.new(|cx| TextEditor::new("Filter by name or description", 1, cx).single_line());
        let subscription = cx.subscribe(&filter, |_, _, _: &EditorEvent, cx| cx.notify());
        let mut picker = Self {
            service,
            accounts: Vec::new(),
            selected: 0,
            listing: Listing::NoAccount,
            generation: 0,
            filter,
            scroll: UniformListScrollHandle::new(),
            task: None,
            _subscription: subscription,
        };
        picker.refresh_accounts(cx);
        picker
    }

    /// Re-reads the account list (after the accounts screen changed it) and
    /// keeps the selection when the account is still there.
    pub fn refresh_accounts(&mut self, cx: &mut Context<Self>) {
        let keep = self.accounts.get(self.selected).map(ForgeAccount::key);
        self.accounts = self.service.accounts();
        self.selected = keep
            .clone()
            .and_then(|key| self.accounts.iter().position(|a| a.key() == key))
            .unwrap_or(0);
        if self.accounts.is_empty() {
            self.generation += 1;
            self.task = None;
            self.listing = Listing::NoAccount;
        } else if matches!(self.listing, Listing::NoAccount | Listing::Failed(_))
            || keep != self.accounts.get(self.selected).map(ForgeAccount::key)
        {
            self.load(false, cx);
        }
        cx.notify();
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if index != self.selected && index < self.accounts.len() {
            self.selected = index;
            self.load(false, cx);
        }
    }

    /// Loads the first pages, or (with `more`) the pages after the ones shown.
    fn load(&mut self, more: bool, cx: &mut Context<Self>) {
        let Some(account) = self.accounts.get(self.selected) else {
            return;
        };
        let key = account.key();
        let (mut repos, cursor) = match (&self.listing, more) {
            (
                Listing::Loaded {
                    repos,
                    more: Some(cursor),
                },
                true,
            ) => (repos.clone(), Some(cursor.clone())),
            _ => (Vec::new(), None),
        };
        self.generation += 1;
        let generation = self.generation;
        self.listing = Listing::Loading;
        let service = self.service.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { load_pages(&service, &key, cursor) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.listing = match result {
                    Ok((mut page, more)) => {
                        repos.append(&mut page);
                        Listing::Loaded { repos, more }
                    }
                    Err(error) => Listing::Failed(error),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

/// Up to `PAGES_PER_LOAD` pages from `cursor` (the first page when `None`).
fn load_pages(
    service: &ForgeService,
    key: &str,
    mut cursor: Option<String>,
) -> Result<(Vec<RemoteRepo>, Option<String>), ForgeError> {
    let mut repos = Vec::new();
    for _ in 0..PAGES_PER_LOAD {
        let page = service.list_repos_page(key, cursor.as_deref())?;
        repos.extend(page.repos);
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    Ok((repos, cursor))
}

impl Render for RepoPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = cx.palette();
        let mut body = div().flex().flex_col().gap(px(SPACE_3));
        let manage = cx.entity();
        let manage_button = move |label: &'static str| {
            let manage = manage.clone();
            Button::new("picker-manage", label).on_click(move |_, _, cx| {
                manage.update(cx, |_, cx| cx.emit(PickerEvent::ManageAccounts))
            })
        };

        if self.accounts.is_empty() {
            return body
                .child(
                    div()
                        .text_style(&TEXT_BODY, cx)
                        .text_color(hsla(p.ink_muted))
                        .child("No account connected. Connect GitHub or GitLab to choose from your repositories, or enter an address instead."),
                )
                .child(div().flex().child(manage_button("Connect an account…").primary()))
                .into_any_element();
        }

        let pick_account = cx.entity();
        body = body.child(
            div()
                .flex()
                .items_center()
                .gap(px(SPACE_2))
                .child(Segmented::new(
                    "picker-account",
                    self.accounts.iter().map(|a| a.label().into()).collect(),
                    self.selected,
                    move |index, _, cx| pick_account.update(cx, |this, cx| this.select(index, cx)),
                ))
                .child(div().flex_1())
                .child(manage_button("Accounts…").ghost()),
        );

        if let Listing::Failed(error) = &self.listing {
            let mut banner = Banner::new(BannerKind::Error, error.user_message());
            if error.needs_reconnect() {
                banner = banner.action(manage_button("Reconnect…"));
            } else {
                let retry = cx.entity();
                banner =
                    banner.action(Button::new("picker-retry", "Try again").on_click(
                        move |_, _, cx| retry.update(cx, |this, cx| this.load(false, cx)),
                    ));
            }
            return body.child(banner).into_any_element();
        }

        body = body.child(self.filter.clone());
        let query = self.filter.read(cx).text().to_owned();
        let shown = filter_repos(self.listing.repos(), &query);
        body = body.child(
            div()
                .text_style(&TEXT_SMALL, cx)
                .text_color(hsla(p.ink_muted))
                .child(summary(&self.listing, shown.len())),
        );

        let repos: Vec<RemoteRepo> = shown
            .iter()
            .map(|&at| self.listing.repos()[at].clone())
            .collect();
        if !repos.is_empty() {
            let rows = repos.len();
            let pick = cx.entity();
            let list = uniform_list("picker-repos", rows, move |range, _, cx| {
                let p = cx.palette();
                let hover = hsla(p.bg_hover);
                range
                    .map(|ix| {
                        let repo = repos[ix].clone();
                        let pick = pick.clone();
                        let chosen = repo.clone();
                        div()
                            .id(("picker-repo", ix))
                            .flex()
                            .items_center()
                            .gap(px(SPACE_3))
                            .h(px(ROW_LIST))
                            .px(px(SPACE_2))
                            .rounded(px(RADIUS_SM))
                            .cursor_pointer()
                            .hover(move |style| style.bg(hover))
                            .on_click(move |_, _, cx| {
                                let repo = chosen.clone();
                                pick.update(cx, |_, cx| cx.emit(PickerEvent::Picked(repo)))
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .max_w(px(260.))
                                    .truncate()
                                    .text_style(&TEXT_BODY, cx)
                                    .text_color(hsla(p.ink))
                                    .child(repo.full_name.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_style(&TEXT_SMALL, cx)
                                    .text_color(hsla(p.ink_muted))
                                    .child(repo.description.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_style(&TEXT_SMALL, cx)
                                    .text_color(hsla(p.ink_faint))
                                    .child(visibility_label(repo.visibility)),
                            )
                    })
                    .collect()
            })
            .track_scroll(&self.scroll);
            body = body.child(
                div()
                    .h(px(rows.min(VISIBLE_ROWS) as f32 * ROW_LIST))
                    .child(list),
            );
        }
        if let Listing::Loaded { more: Some(_), .. } = &self.listing {
            let more = cx.entity();
            body = body.child(
                div().flex().child(
                    Button::new("picker-more", "Load more")
                        .on_click(move |_, _, cx| more.update(cx, |this, cx| this.load(true, cx))),
                ),
            );
        }
        body.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> RemoteRepo {
        RemoteRepo {
            full_name: name.into(),
            clone_url: format!("https://example.com/{name}.git"),
            description: String::new(),
            visibility: Visibility::Private,
            default_branch: None,
            updated_at: None,
        }
    }

    #[test]
    fn the_summary_counts_what_shows() {
        let loaded = |n: usize, more: bool| Listing::Loaded {
            repos: (0..n).map(|i| repo(&format!("o/r{i}"))).collect(),
            more: more.then(|| "next".to_owned()),
        };
        assert_eq!(summary(&Listing::Loading, 0), "Loading repositories…");
        assert_eq!(summary(&loaded(3, false), 3), "3 repositories");
        assert_eq!(summary(&loaded(3, false), 1), "1 of 3 repositories");
        assert_eq!(
            summary(&loaded(3, true), 3),
            "3 repositories (more available)"
        );
        assert_eq!(summary(&Listing::NoAccount, 0), "");
        assert_eq!(summary(&Listing::Failed(ForgeError::Network), 0), "");
    }

    #[test]
    fn only_a_loaded_listing_has_repos() {
        assert!(Listing::Loading.repos().is_empty());
        assert!(Listing::Failed(ForgeError::Unauthorized).repos().is_empty());
        let loaded = Listing::Loaded {
            repos: vec![repo("a/b")],
            more: None,
        };
        assert_eq!(loaded.repos().len(), 1);
    }
}
