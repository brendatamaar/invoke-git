//! The forge service: the connected accounts, their tokens and the calls
//! that use them. One value, cheap to clone, shared by the UI and the
//! sign-in runner.
//!
//! - The account list (who is connected where; no secret) is kept in a text
//!   file in the app data folder.
//! - Each token is kept in a [`TokenStore`] (the OS keychain in the app).
//! - Nothing here is needed by git itself: with no account, clone, fetch and
//!   push work exactly as before.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use invoke_core::forge::{
    AccountBook, AuthMethod, CreatePrInput, ForgeAccount, ForgeKind, Origin, PrCreateTarget,
    PrFilter, PullRequest, RemoteRepo, Resolution, ReviewStatus,
};

use super::api::{self, GitHubUrls, PrPage, RepoPage};
use super::error::ForgeError;
use super::http::Http;
use super::secrets::{Credential, Secret, TokenStore};

/// The OAuth app registered for Invoke Git on GitHub (a public identifier,
/// not a secret; the device flow has no client secret).
///
/// SPEC GAP (P8): the real value is not known yet. Until the maintainers
/// register the app and fill this in, GitHub sign-in answers
/// `NotConfigured` and a personal access token still works.
/// `INVOKE_GIT_GITHUB_CLIENT_ID` overrides it for development.
pub const GITHUB_OAUTH_CLIENT_ID: Option<&str> = None;

/// The OAuth application registered on gitlab.com (public identifier; PKCE
/// needs no secret). Same spec gap as above;
/// `INVOKE_GIT_GITLAB_CLIENT_ID` overrides it for development.
pub const GITLAB_OAUTH_CLIENT_ID: Option<&str> = None;

/// The loopback port the GitLab application's redirect address names:
/// `http://127.0.0.1:47821/callback`. GitLab matches the redirect address
/// exactly, so it cannot be a random port.
pub const GITLAB_LOOPBACK_PORT: u16 = 47_821;

/// A margin so a token is refreshed before it runs out mid-request.
const REFRESH_MARGIN_SECS: u64 = 60;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientIds {
    pub github: Option<String>,
    pub gitlab: Option<String>,
}

impl ClientIds {
    /// The ids built into the app, or the ones named by the environment.
    pub fn from_build() -> Self {
        let pick = |var: &str, built: Option<&str>| {
            std::env::var(var)
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .or_else(|| built.map(str::to_owned))
        };
        Self {
            github: pick("INVOKE_GIT_GITHUB_CLIENT_ID", GITHUB_OAUTH_CLIENT_ID),
            gitlab: pick("INVOKE_GIT_GITLAB_CLIENT_ID", GITLAB_OAUTH_CLIENT_ID),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub github: GitHubUrls,
    pub client_ids: ClientIds,
    /// 0 asks the OS for a free port (tests); the app uses
    /// [`GITLAB_LOOPBACK_PORT`].
    pub gitlab_loopback_port: u16,
    /// How long one "second" of a poll interval lasts. Tests shrink it.
    pub poll_unit: Duration,
    /// The GitLab server the OAuth (browser) sign-in talks to.
    pub gitlab_instance: String,
    /// How long the browser sign-in waits for the redirect.
    pub browser_timeout: Duration,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            github: GitHubUrls::default(),
            client_ids: ClientIds::from_build(),
            gitlab_loopback_port: GITLAB_LOOPBACK_PORT,
            poll_unit: Duration::from_secs(1),
            gitlab_instance: "https://gitlab.com".into(),
            browser_timeout: Duration::from_secs(300),
        }
    }
}

/// Repositories listed so far, and where the rest would start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    pub repos: Vec<RemoteRepo>,
    /// `Some` when the page limit stopped the listing early.
    pub more: Option<String>,
}

/// Pull/merge requests listed so far, and where the rest would start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrListing {
    pub prs: Vec<PullRequest>,
    /// `Some` when the page limit stopped the listing early.
    pub more: Option<String>,
}

/// One sign-in's say over whether it may still commit, shared by the threads
/// of the flow and the runner's cancel.
///
/// Cancel and commit exclude each other, and cancel never waits for a commit:
///
/// - cancelled first: the flow stores nothing
/// - cancelled while the flow is writing (`Committing`): cancel only leaves a
///   note, and the flow undoes its own write when it is done
/// - cancelled after the commit: cancel undoes exactly that commit
///
/// So the "cancelled" on screen and the stored state agree, and a store that
/// calls back into cancel from inside a write (the same thread) cannot
/// deadlock: the gate is only ever held for a moment, never across IO.
#[derive(Default)]
pub(super) struct FlowControl {
    /// A fast flag for loops and for suppressing late events.
    cancelled: AtomicBool,
    gate: Mutex<Gate>,
}

#[derive(Default)]
enum Gate {
    #[default]
    Open,
    /// The flow is writing its account.
    Committing {
        cancel_requested: bool,
    },
    /// The flow stored its account; kept so cancel can undo only this write.
    Committed(Box<Commit>),
    Cancelled,
}

pub(super) struct Commit {
    account: ForgeAccount,
    written: Credential,
    previous_account: Option<ForgeAccount>,
    previous_credential: Option<Credential>,
}

impl FlowControl {
    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn gate(&self) -> MutexGuard<'_, Gate> {
        self.gate.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The flow is over or replaced: it may not start a commit any more, but
    /// what it already committed (or is committing) stays: the user saw it.
    pub(super) fn retire(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let mut gate = self.gate();
        if matches!(*gate, Gate::Open) {
            *gate = Gate::Cancelled;
        }
    }
}

struct Inner {
    http: Http,
    store: Arc<dyn TokenStore>,
    config: ServiceConfig,
    /// The working list, held for the whole of a change (keychain calls
    /// included). Never taken by `accounts` or `book`, which the UI calls.
    book: Mutex<AccountBook>,
    /// A copy of the list as last read or written, for the UI: locked only to
    /// copy it, never across IO.
    snapshot: Mutex<AccountBook>,
    /// The list file's text as this service last read or wrote it (`None`:
    /// no file). The expected state every write is checked against.
    on_disk: Mutex<Option<String>>,
    book_path: Option<PathBuf>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

#[derive(Clone)]
pub struct ForgeService {
    inner: Arc<Inner>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl ForgeService {
    /// Opens the service. `book_path` is the account list file (`None`
    /// keeps the list in memory only, for tests). A missing file is an empty
    /// list; an unreadable one is an error, so a bad read never leads to
    /// overwriting a good list.
    pub fn new(
        store: Arc<dyn TokenStore>,
        config: ServiceConfig,
        book_path: Option<PathBuf>,
    ) -> Result<Self, ForgeError> {
        let on_disk = match &book_path {
            Some(path) => read_disk(path)?,
            None => None,
        };
        let book = AccountBook::decode(on_disk.as_deref().unwrap_or(""));
        Ok(Self {
            inner: Arc::new(Inner {
                http: Http::new()?,
                store,
                config,
                snapshot: Mutex::new(book.clone()),
                book: Mutex::new(book),
                on_disk: Mutex::new(on_disk),
                book_path,
                clock: Box::new(unix_now),
            }),
        })
    }

    /// The same service with its own clock (tests).
    pub fn with_clock(
        store: Arc<dyn TokenStore>,
        config: ServiceConfig,
        book_path: Option<PathBuf>,
        clock: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Result<Self, ForgeError> {
        let service = Self::new(store, config, book_path)?;
        let inner = Arc::try_unwrap(service.inner)
            .map_err(|_| ForgeError::Unavailable("service already shared".into()))?;
        Ok(Self {
            inner: Arc::new(Inner {
                clock: Box::new(clock),
                ..inner
            }),
        })
    }

    pub(super) fn http(&self) -> &Http {
        &self.inner.http
    }

    pub(super) fn config(&self) -> &ServiceConfig {
        &self.inner.config
    }

    pub(super) fn now(&self) -> u64 {
        (self.inner.clock)()
    }

    /// Takes the working list for reading, refreshed from disk first (the
    /// disk is the source of truth: another window or process may have
    /// changed it). Lock order: the list, the lock file, `on_disk`, the
    /// snapshot.
    fn book_lock(&self) -> MutexGuard<'_, AccountBook> {
        let mut book = self
            .inner
            .book
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.reload_into(&mut book);
        book
    }

    /// Takes the working list for a change: this process's lock, then the
    /// lock file every process shares, then a fresh read. Held for the whole
    /// read-check-write of the list and the keychain, so no other window or
    /// process can interleave with it.
    fn begin_write(&self) -> Result<Writing<'_>, ForgeError> {
        let book = self
            .inner
            .book
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let file = match &self.inner.book_path {
            Some(path) => Some(lock_file(path)?),
            None => None,
        };
        let mut writing = Writing { _file: file, book };
        self.reload_into(&mut writing.book);
        Ok(writing)
    }

    fn reload_into(&self, book: &mut AccountBook) {
        if let Some(path) = &self.inner.book_path
            && let Ok(text) = read_disk(path)
        {
            let mut on_disk = self.on_disk();
            if *on_disk != text {
                *book = AccountBook::decode(text.as_deref().unwrap_or(""));
                *on_disk = text;
            }
        }
        *self.snapshot() = book.clone();
    }

    fn on_disk(&self) -> MutexGuard<'_, Option<String>> {
        self.inner
            .on_disk
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn snapshot(&self) -> MutexGuard<'_, AccountBook> {
        self.inner
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // -- the account list ----------------------------------------------------

    /// The accounts as last read or written. No IO and no waiting on a
    /// change in progress: safe to call while drawing.
    pub fn accounts(&self) -> Vec<ForgeAccount> {
        self.snapshot().accounts().to_vec()
    }

    /// The whole list as last read or written (see [`accounts`](Self::accounts)).
    pub fn book(&self) -> AccountBook {
        self.snapshot().clone()
    }

    /// Reads the list from disk again, for changes made by another process.
    /// Does IO: call it off the UI thread.
    pub fn reload(&self) {
        drop(self.book_lock());
    }

    /// Which account serves a remote address.
    pub fn resolve(&self, remote_url: &str) -> Resolution {
        self.book_lock().resolve(remote_url)
    }

    /// Makes `key` the account used for its host. The list in memory only
    /// changes once the file has.
    pub fn prefer(&self, key: &str) -> Result<(), ForgeError> {
        let mut writing = self.begin_write()?;
        let mut new = writing.book.clone();
        if !new.prefer(key) {
            return Err(ForgeError::UnknownAccount);
        }
        self.commit_book(&mut writing.book, new)
    }

    /// Writes `new` and only then makes it the list in memory (and the UI's
    /// copy). A refused write leaves memory as the disk is.
    fn commit_book(&self, book: &mut AccountBook, new: AccountBook) -> Result<(), ForgeError> {
        self.save_book(&new)?;
        *self.snapshot() = new.clone();
        *book = new;
        Ok(())
    }

    /// Writes the list, but only over the text this service last saw: if the
    /// file changed since (another window or process), nothing is written
    /// and the caller rolls back, so no one's account is silently dropped.
    fn save_book(&self, book: &AccountBook) -> Result<(), ForgeError> {
        let Some(path) = &self.inner.book_path else {
            return Ok(());
        };
        let mut on_disk = self.on_disk();
        if read_disk(path)? != *on_disk {
            return Err(ForgeError::Disk(
                "the account list changed on disk meanwhile; try again".into(),
            ));
        }
        let text = book.encode();
        save_book(path, &text)?;
        *on_disk = Some(text);
        Ok(())
    }

    // -- connecting ------------------------------------------------------------

    /// Signs in with a personal access token the user pasted. The token is
    /// checked against the server first; only a working token is kept.
    /// `instance` is the GitLab server (empty means gitlab.com); GitHub is
    /// always github.com.
    pub fn connect_with_token(
        &self,
        kind: ForgeKind,
        instance: &str,
        token: &str,
    ) -> Result<ForgeAccount, ForgeError> {
        let secret = Secret::new(token).ok_or(ForgeError::InvalidToken)?;
        let origin = self.origin_for_new(kind, instance)?;
        let credential = Credential::token(secret);
        let username = self.user_of(kind, &origin, &credential)?;
        self.register(
            kind,
            origin,
            &username,
            AuthMethod::Pat,
            credential,
            &FlowControl::default(),
        )
    }

    /// Records an account whose OAuth tokens were just obtained. The flow's
    /// gate decides: if the user cancelled first nothing is stored, and if
    /// this commits first a later cancel undoes exactly this write.
    pub(super) fn finish_oauth(
        &self,
        kind: ForgeKind,
        origin: Origin,
        credential: Credential,
        flow: &FlowControl,
    ) -> Result<ForgeAccount, ForgeError> {
        if flow.is_cancelled() {
            return Err(ForgeError::Cancelled);
        }
        let username = self.user_of(kind, &origin, &credential)?;
        self.register(kind, origin, &username, AuthMethod::OAuth, credential, flow)
    }

    /// Marks `flow` cancelled without ever waiting on IO or file locks (see
    /// [`FlowControl`]). Returns the finished [`Commit`] if this flow already
    /// committed, so the caller can run [`undo_commit`](Self::undo_commit) on
    /// a background thread.
    pub(super) fn begin_cancel(&self, flow: &FlowControl) -> Option<Box<Commit>> {
        flow.cancelled.store(true, Ordering::SeqCst);
        let mut gate = flow.gate();
        match std::mem::replace(&mut *gate, Gate::Cancelled) {
            Gate::Committed(commit) => Some(commit),
            Gate::Committing { .. } => {
                // The flow undoes its own write when it finishes.
                *gate = Gate::Committing {
                    cancel_requested: true,
                };
                None
            }
            Gate::Open | Gate::Cancelled => None,
        }
    }

    /// Undoes a finished commit returned by [`begin_cancel`](Self::begin_cancel).
    /// Blocks on the OS lock and keychain; call off the UI thread.
    pub(super) fn undo_commit(&self, commit: &Commit) -> Result<(), ForgeError> {
        let mut writing = self.begin_write().map_err(|_| ForgeError::RollbackFailed)?;
        self.undo_locked(&mut writing.book, commit)
    }

    /// Puts back what `commit` replaced, if it is still what the flow wrote.
    /// The caller holds the list lock.
    fn undo_locked(&self, book: &mut AccountBook, commit: &Commit) -> Result<(), ForgeError> {
        let key = commit.account.key();
        if book.get(&key) != Some(&commit.account) {
            return Ok(());
        }
        // A keychain that cannot be read leaves the outcome unknown: that is
        // a failed undo to report, not "nothing to undo".
        let current = self
            .inner
            .store
            .load(&key)
            .map_err(|_| ForgeError::RollbackFailed)?;
        let still_ours = current.as_ref() == Some(&commit.written);
        if !still_ours {
            return Ok(());
        }
        let mut new = book.clone();
        match &commit.previous_account {
            Some(previous) => new.add(previous.clone()),
            None => {
                new.remove(&key);
            }
        }
        // The list first, like a disconnect: a restart must not find an
        // account whose token is gone. If the list cannot be written, stop
        // before the keychain: list and token still agree, and the failure
        // is reported.
        self.commit_book(book, new)
            .map_err(|_| ForgeError::RollbackFailed)?;
        match &commit.previous_credential {
            Some(previous) => self.inner.store.save(&key, previous),
            None => self.inner.store.delete(&key),
        }
        .map_err(|_| ForgeError::RollbackFailed)
    }

    pub(super) fn origin_for_new(
        &self,
        kind: ForgeKind,
        instance: &str,
    ) -> Result<Origin, ForgeError> {
        let text = match (kind, instance.trim()) {
            (ForgeKind::GitHub, _) => "https://github.com",
            (ForgeKind::GitLab, "") => "https://gitlab.com",
            (ForgeKind::GitLab, other) => other,
        };
        Origin::parse(text).map_err(ForgeError::BadOrigin)
    }

    fn user_of(
        &self,
        kind: ForgeKind,
        origin: &Origin,
        credential: &Credential,
    ) -> Result<String, ForgeError> {
        let token = &credential.access;
        let username = match kind {
            ForgeKind::GitHub => {
                api::github::current_user(&self.inner.http, &self.inner.config.github.api, token)?
            }
            ForgeKind::GitLab => api::gitlab::current_user(&self.inner.http, origin, token)?,
        };
        if acceptable_username(&username, credential) {
            Ok(username)
        } else {
            Err(ForgeError::BadResponse("unusable user name"))
        }
    }

    /// Keychain first, then the list: an account never appears without its
    /// token. The whole sequence (look at what is there, write the token,
    /// write the list) runs under the list lock, so a disconnect or another
    /// sign-in cannot interleave with it, and the flow's gate says what to do
    /// about a cancel that arrives meanwhile. If the list cannot be written the token is put
    /// back as it was, and if even that fails the error says so, because a
    /// token may then be left behind.
    fn register(
        &self,
        kind: ForgeKind,
        origin: Origin,
        username: &str,
        method: AuthMethod,
        credential: Credential,
        flow: &FlowControl,
    ) -> Result<ForgeAccount, ForgeError> {
        let account = ForgeAccount::new(kind, origin, username, method)
            .ok_or(ForgeError::BadResponse("unusable user name"))?;
        {
            // Only a moment under the gate: it is never held across IO.
            let mut gate = flow.gate();
            match *gate {
                Gate::Open => {
                    *gate = Gate::Committing {
                        cancel_requested: false,
                    }
                }
                _ => return Err(ForgeError::Cancelled),
            }
        }
        let mut writing = match self.begin_write() {
            Ok(writing) => writing,
            Err(error) => {
                *flow.gate() = Gate::Cancelled;
                return Err(error);
            }
        };
        let written = self.write_account(&mut writing.book, &account, &credential);
        let commit = match written {
            Ok(commit) => commit,
            Err(error) => {
                *flow.gate() = Gate::Cancelled;
                return Err(error);
            }
        };
        let mut gate = flow.gate();
        match std::mem::replace(&mut *gate, Gate::Cancelled) {
            Gate::Committing {
                cancel_requested: false,
            } => {
                *gate = Gate::Committed(Box::new(commit));
                Ok(account)
            }
            // Cancelled while writing: undo this write now.
            _ => {
                drop(gate);
                self.undo_locked(&mut writing.book, &commit)?;
                Err(ForgeError::Cancelled)
            }
        }
    }

    /// Writes the token and then the list, putting the token back as it was
    /// if the list cannot be written (and saying so if even that fails).
    /// The caller holds the list lock.
    fn write_account(
        &self,
        book: &mut AccountBook,
        account: &ForgeAccount,
        credential: &Credential,
    ) -> Result<Commit, ForgeError> {
        let key = account.key();
        let previous_account = book.get(&key).cloned();
        let previous_credential = self.inner.store.load(&key)?;
        self.inner.store.save(&key, credential)?;
        let mut new = book.clone();
        new.add(account.clone());
        if let Err(error) = self.commit_book(book, new) {
            // Put the token back only if it is still the one written here:
            // never delete or replace a credential someone else wrote.
            let token_back = match self.inner.store.load(&key) {
                Ok(Some(current)) if current == *credential => match &previous_credential {
                    Some(previous) => self.inner.store.save(&key, previous),
                    None => self.inner.store.delete(&key),
                },
                Ok(_) => Ok(()),
                Err(error) => Err(error),
            };
            return Err(if token_back.is_err() {
                ForgeError::RollbackFailed
            } else {
                error
            });
        }
        Ok(Commit {
            account: account.clone(),
            written: credential.clone(),
            previous_account,
            previous_credential,
        })
    }

    /// Forgets an account. The list changes first, so a restart cannot bring
    /// back an account whose token is gone; then the token leaves the
    /// keychain. If the keychain refuses, the account is put back (so the
    /// token is not left unseen) and the error is reported.
    pub fn disconnect(&self, key: &str) -> Result<(), ForgeError> {
        let mut writing = self.begin_write()?;
        let before = writing.book.clone();
        let mut new = before.clone();
        if new.remove(key).is_none() {
            return Err(ForgeError::UnknownAccount);
        }
        self.commit_book(&mut writing.book, new)?;
        if let Err(error) = self.inner.store.delete(key) {
            return match self.commit_book(&mut writing.book, before) {
                Ok(()) => Err(error),
                Err(_) => Err(ForgeError::RollbackFailed),
            };
        }
        Ok(())
    }

    // -- using the token ------------------------------------------------

    fn account(&self, key: &str) -> Result<ForgeAccount, ForgeError> {
        self.book_lock()
            .get(key)
            .cloned()
            .ok_or(ForgeError::UnknownAccount)
    }

    /// The account's token, refreshed first when it is about to run out.
    fn credential(&self, account: &ForgeAccount) -> Result<Credential, ForgeError> {
        let key = account.key();
        let credential = self
            .inner
            .store
            .load(&key)?
            .ok_or(ForgeError::NoCredential)?;
        if credential.expires_within(self.now(), REFRESH_MARGIN_SECS) {
            return self.refresh(account, &credential);
        }
        Ok(credential)
    }

    fn refresh(&self, account: &ForgeAccount, old: &Credential) -> Result<Credential, ForgeError> {
        let (Some(refresh), ForgeKind::GitLab) = (&old.refresh, account.kind) else {
            return Err(ForgeError::Unauthorized);
        };
        let client_id = self
            .inner
            .config
            .client_ids
            .gitlab
            .as_deref()
            .ok_or(ForgeError::NotConfigured(ForgeKind::GitLab))?;
        let mut fresh = api::gitlab::refresh(
            &self.inner.http,
            &account.origin,
            client_id,
            refresh,
            self.now(),
        )?;
        // A server may leave the refresh token out of the answer; keep ours.
        if fresh.refresh.is_none() {
            fresh.refresh = old.refresh.clone();
        }
        // Write only if the account is still connected and its token is still
        // the one this refresh started from. Holding the list lock keeps a
        // disconnect from interleaving, so a refresh can never put back the
        // keychain entry of an account that was just removed.
        let key = account.key();
        let writing = self.begin_write()?;
        if writing.book.get(&key).is_none() {
            return Err(ForgeError::UnknownAccount);
        }
        match self.inner.store.load(&key)? {
            Some(current) if &current == old => {
                self.inner.store.save(&key, &fresh)?;
                Ok(fresh)
            }
            // Something else replaced it meanwhile (another refresh, or a new
            // sign-in): that one is newer than ours.
            Some(current) => Ok(current),
            None => Err(ForgeError::NoCredential),
        }
    }

    /// One page of the account's repositories. A token the server rejects is
    /// refreshed once (GitLab OAuth) before giving up.
    pub fn list_repos_page(&self, key: &str, cursor: Option<&str>) -> Result<RepoPage, ForgeError> {
        let account = self.account(key)?;
        let credential = self.credential(&account)?;
        match self.page(&account, &credential, cursor) {
            Err(ForgeError::Unauthorized) if credential.refresh.is_some() => {
                let fresh = self.refresh(&account, &credential)?;
                self.page(&account, &fresh, cursor)
            }
            other => other,
        }
    }

    fn page(
        &self,
        account: &ForgeAccount,
        credential: &Credential,
        cursor: Option<&str>,
    ) -> Result<RepoPage, ForgeError> {
        let token = &credential.access;
        let mut page = match account.kind {
            ForgeKind::GitHub => api::github::list_repos_page(
                &self.inner.http,
                &self.inner.config.github.api,
                token,
                cursor,
            ),
            ForgeKind::GitLab => {
                api::gitlab::list_repos_page(&self.inner.http, &account.origin, token, cursor)
            }
        }?;
        // Every field of a repo is shown or used (the clone address ends up
        // in the new repo's config): a repo where any of them holds the
        // access or the refresh token, in any case, is dropped.
        let secrets: Vec<String> = std::iter::once(&credential.access)
            .chain(credential.refresh.as_ref())
            .map(|secret| secret.expose().to_ascii_lowercase())
            .collect();
        page.repos.retain(|repo| {
            let fields = [
                Some(repo.full_name.as_str()),
                Some(repo.clone_url.as_str()),
                Some(repo.description.as_str()),
                repo.default_branch.as_deref(),
                repo.updated_at.as_deref(),
            ];
            !fields.into_iter().flatten().any(|field| {
                let field = field.to_ascii_lowercase();
                secrets.iter().any(|secret| field.contains(secret))
            })
        });
        Ok(page)
    }

    /// Up to `max_pages` pages, newest activity first. When the limit stops
    /// the listing, `more` holds where it would continue.
    pub fn list_repos(&self, key: &str, max_pages: usize) -> Result<Listing, ForgeError> {
        let mut repos = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..max_pages.max(1) {
            let page = self.list_repos_page(key, cursor.as_deref())?;
            repos.extend(page.repos);
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        Ok(Listing {
            repos,
            more: cursor,
        })
    }

    // -- pull requests & merge requests (P9) ----------------------------------

    /// Runs an authenticated call, refreshing an expired/rejected GitLab OAuth
    /// token once before giving up.
    fn with_token<T>(
        &self,
        key: &str,
        call: impl Fn(&ForgeAccount, &Credential) -> Result<T, ForgeError>,
    ) -> Result<T, ForgeError> {
        let account = self.account(key)?;
        let credential = self.credential(&account)?;
        match call(&account, &credential) {
            Err(ForgeError::Unauthorized) if credential.refresh.is_some() => {
                let fresh = self.refresh(&account, &credential)?;
                call(&account, &fresh)
            }
            other => other,
        }
    }

    /// One page of pull/merge requests for `repo_path`.
    pub fn list_prs_page(
        &self,
        key: &str,
        repo_path: &str,
        filter: PrFilter,
        cursor: Option<&str>,
    ) -> Result<PrPage, ForgeError> {
        self.with_token(key, |account, credential| {
            let mut page = match account.kind {
                ForgeKind::GitHub => {
                    let (owner, repo) = split_github_path(repo_path)?;
                    api::github::list_prs_page(
                        &self.inner.http,
                        &self.inner.config.github.api,
                        &credential.access,
                        owner,
                        repo,
                        filter,
                        cursor,
                    )
                }
                ForgeKind::GitLab => api::gitlab::list_mrs_page(
                    &self.inner.http,
                    &account.origin,
                    &credential.access,
                    repo_path,
                    filter,
                    cursor,
                ),
            }?;
            page.prs.retain(|pr| !pr_contains_secret(pr, credential));
            if page
                .next
                .as_deref()
                .is_some_and(|next| text_contains_secret(next, credential))
            {
                page.next = None;
            }
            Ok(page)
        })
    }

    /// Up to `max_pages` pages of pull/merge requests for `repo_path`.
    pub fn list_prs(
        &self,
        key: &str,
        repo_path: &str,
        filter: PrFilter,
        max_pages: usize,
    ) -> Result<PrListing, ForgeError> {
        let mut prs = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..max_pages.max(1) {
            let page = self.list_prs_page(key, repo_path, filter, cursor.as_deref())?;
            prs.extend(page.prs);
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        Ok(PrListing { prs, more: cursor })
    }

    /// Reads full detail and review/approval status for one PR/MR.
    pub fn get_pr_detail(
        &self,
        key: &str,
        repo_path: &str,
        number: u64,
    ) -> Result<PullRequest, ForgeError> {
        self.with_token(key, |account, credential| {
            let mut pr = match account.kind {
                ForgeKind::GitHub => {
                    let (owner, repo) = split_github_path(repo_path)?;
                    let mut pr = api::github::get_pr(
                        &self.inner.http,
                        &self.inner.config.github.api,
                        &credential.access,
                        owner,
                        repo,
                        number,
                    )?;
                    if let Ok(status) = api::github::list_pr_reviews(
                        &self.inner.http,
                        &self.inner.config.github.api,
                        &credential.access,
                        owner,
                        repo,
                        number,
                    ) && status != ReviewStatus::None
                    {
                        pr.review_status = status;
                    }
                    pr
                }
                ForgeKind::GitLab => {
                    let mut mr = api::gitlab::get_mr(
                        &self.inner.http,
                        &account.origin,
                        &credential.access,
                        repo_path,
                        number,
                    )?;
                    if let Ok(status) = api::gitlab::get_mr_approvals(
                        &self.inner.http,
                        &account.origin,
                        &credential.access,
                        repo_path,
                        number,
                    ) && status != ReviewStatus::None
                    {
                        mr.review_status = status;
                    }
                    mr
                }
            };
            if pr_contains_secret(&pr, credential) {
                return Err(ForgeError::BadResponse("unusable pull request data"));
            }
            // Keep review_status consistent if draft.
            if pr.draft && pr.review_status == ReviewStatus::None {
                pr.review_status = ReviewStatus::Pending;
            }
            Ok(pr)
        })
    }

    /// Creates a new Pull Request (GitHub) or Merge Request (GitLab), supporting
    /// both same-repo and cross-repo (fork) targets.
    pub fn create_pr(
        &self,
        target: &PrCreateTarget,
        input: &CreatePrInput,
    ) -> Result<PullRequest, ForgeError> {
        let key = target.account.key();
        self.with_token(&key, |account, credential| {
            let pr = match account.kind {
                ForgeKind::GitHub => {
                    let (base_owner, base_repo) = split_github_path(&target.base_repo.path)?;
                    let (head, head_repo) = if target.is_fork {
                        let (head_owner, head_repo_name) =
                            split_github_path(&target.head_repo.path)?;
                        (
                            format!("{head_owner}:{}", target.head_branch),
                            Some(head_repo_name),
                        )
                    } else {
                        (target.head_branch.clone(), None)
                    };
                    api::github::create_pr(
                        &self.inner.http,
                        &self.inner.config.github.api,
                        &credential.access,
                        base_owner,
                        base_repo,
                        &head,
                        head_repo,
                        input,
                    )?
                }
                ForgeKind::GitLab => {
                    let target_project_id = if target.is_fork {
                        Some(api::gitlab::get_project_id(
                            &self.inner.http,
                            &account.origin,
                            &credential.access,
                            &target.base_repo.path,
                        )?)
                    } else {
                        None
                    };
                    api::gitlab::create_mr(
                        &self.inner.http,
                        &account.origin,
                        &credential.access,
                        &target.head_repo.path,
                        &target.head_branch,
                        input,
                        target_project_id,
                    )?
                }
            };
            if pr_contains_secret(&pr, credential) {
                return Err(ForgeError::BadResponse("unusable pull request data"));
            }
            Ok(pr)
        })
    }
}

fn split_github_path(path: &str) -> Result<(&str, &str), ForgeError> {
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return Err(ForgeError::BadResponse("invalid repository path"));
    }
    Ok((owner, repo))
}

fn pct_decode_lossy(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            )
        {
            out.push(((hi << 4) | lo) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn text_contains_secret(text: &str, credential: &Credential) -> bool {
    let secrets: Vec<String> = std::iter::once(&credential.access)
        .chain(credential.refresh.as_ref())
        .map(|s| s.expose().to_ascii_lowercase())
        .collect();
    let lower = text.to_ascii_lowercase();
    if secrets.iter().any(|secret| lower.contains(secret)) {
        return true;
    }
    let decoded_lower = pct_decode_lossy(text).to_ascii_lowercase();
    secrets.iter().any(|secret| decoded_lower.contains(secret))
}

fn pr_contains_secret(pr: &PullRequest, credential: &Credential) -> bool {
    let fields = [
        Some(pr.title.as_str()),
        Some(pr.body.as_str()),
        Some(pr.author.as_str()),
        Some(pr.head_branch.as_str()),
        pr.head_repo.as_deref(),
        pr.head_sha.as_deref(),
        Some(pr.base_branch.as_str()),
        Some(pr.web_url.as_str()),
        pr.updated_at.as_deref(),
    ];
    fields
        .into_iter()
        .flatten()
        .any(|field| text_contains_secret(field, credential))
}

/// The working list held for a change. Fields drop in order: the lock file
/// (other processes) is released before this process's lock.
struct Writing<'a> {
    _file: Option<std::fs::File>,
    book: MutexGuard<'a, AccountBook>,
}

/// Takes the lock file next to the list (`forge-accounts.lock`), shared by
/// every process: an OS lock on the file, released when the handle closes.
fn lock_file(path: &Path) -> Result<std::fs::File, ForgeError> {
    let disk = |_| ForgeError::Disk("the account list could not be locked".into());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(disk)?;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .map_err(disk)?;
    file.lock().map_err(disk)?;
    Ok(file)
}

/// The list file's text, or `None` when there is no file yet.
fn read_disk(path: &Path) -> Result<Option<String>, ForgeError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(ForgeError::Disk(
            "the account list could not be read".into(),
        )),
    }
}

/// Writes the list through a temporary file so a crash never leaves half of
/// it. It holds names and hosts only.
fn save_book(path: &Path, text: &str) -> Result<(), ForgeError> {
    let disk = |_| ForgeError::Disk("the data folder is not writable".into());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(disk)?;
    }
    // A name of its own per write, so two writers never share a temporary.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        WRITES.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&temporary, text).map_err(disk)?;
    std::fs::rename(&temporary, path).map_err(disk)
}

/// A user name a forge may report: ASCII letters, digits, `-`, `_` and `.`,
/// at most 100 of them, and never the access or refresh token (compared
/// without regard to case, or anything that holds either): the name is shown and written to the account list, so a hostile
/// server must not be able to echo a token there.
pub(crate) fn acceptable_username(name: &str, credential: &Credential) -> bool {
    let lower = name.to_ascii_lowercase();
    let holds = |secret: &Secret| lower.contains(&secret.expose().to_ascii_lowercase());
    !name.is_empty()
        && name.len() <= 100
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !holds(&credential.access)
        && !credential.refresh.as_ref().is_some_and(holds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(access: &str, refresh: Option<&str>) -> Credential {
        Credential {
            access: Secret::new(access).unwrap(),
            refresh: refresh.and_then(Secret::new),
            expires_at: None,
        }
    }

    #[test]
    fn a_user_name_never_holds_either_token_in_any_case() {
        let both = credential("glpat-Access1", Some("glrt-Refresh1"));
        for fine in ["ana", "ana.gl", "Ana_Corp-2", "glpat", "refresh"] {
            assert!(acceptable_username(fine, &both), "{fine}");
        }
        for bad in [
            "glpat-Access1",
            "GLPAT-ACCESS1",
            "x-glpat-access1-x",
            "glrt-Refresh1",
            "GLRT-REFRESH1",
            "a.glrt-refresh1",
            "",
            "has space",
            "a/b",
        ] {
            assert!(!acceptable_username(bad, &both), "{bad}");
        }
        // Without a refresh token only the access token counts.
        let plain = credential("tok", None);
        assert!(acceptable_username("glrt-Refresh1", &plain));
        assert!(!acceptable_username("myTOKen", &plain));
    }
}
