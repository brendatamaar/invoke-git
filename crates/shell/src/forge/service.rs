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
    AccountBook, AuthMethod, ForgeAccount, ForgeKind, Origin, RemoteRepo, Resolution,
};

use super::api::{self, GitHubUrls, RepoPage};
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

/// One sign-in's say over whether it may still commit, shared by the threads
/// of the flow and the runner's cancel.
///
/// Cancel and commit exclude each other: a commit happens entirely under the
/// gate, and cancel takes the same gate. So either the flow was cancelled
/// first and stores nothing, or it committed first and cancel undoes exactly
/// that commit. The result on screen (cancelled) and the stored state agree.
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
    Cancelled,
    /// The flow stored its account; kept so cancel can undo only this write.
    Committed(Box<Commit>),
}

struct Commit {
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

    /// The flow is over or replaced: it may not commit any more, but what it
    /// already committed stays (it was shown to the user).
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
    book: Mutex<AccountBook>,
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
        let book = match &book_path {
            Some(path) => load_book(path)?,
            None => AccountBook::default(),
        };
        Ok(Self {
            inner: Arc::new(Inner {
                http: Http::new()?,
                store,
                config,
                book: Mutex::new(book),
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

    fn book_lock(&self) -> MutexGuard<'_, AccountBook> {
        self.inner
            .book
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // -- the account list ----------------------------------------------------

    pub fn accounts(&self) -> Vec<ForgeAccount> {
        self.book_lock().accounts().to_vec()
    }

    pub fn book(&self) -> AccountBook {
        self.book_lock().clone()
    }

    /// Which account serves a remote address.
    pub fn resolve(&self, remote_url: &str) -> Resolution {
        self.book_lock().resolve(remote_url)
    }

    /// Makes `key` the account used for its host.
    pub fn prefer(&self, key: &str) -> Result<(), ForgeError> {
        let mut book = self.book_lock();
        if !book.prefer(key) {
            return Err(ForgeError::UnknownAccount);
        }
        self.save_book(&book)
    }

    fn save_book(&self, book: &AccountBook) -> Result<(), ForgeError> {
        match &self.inner.book_path {
            Some(path) => save_book(path, book),
            None => Ok(()),
        }
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

    /// Cancels a flow. If it already committed (and the user was not yet
    /// told), that commit is undone, so what is stored matches the
    /// "cancelled" the user sees. Undoing is compare-and-restore: it only
    /// touches the account and token this flow wrote, and only if they are
    /// still as it left them (a later sign-in or disconnect wins).
    pub(super) fn cancel_flow(&self, flow: &FlowControl) {
        flow.cancelled.store(true, Ordering::SeqCst);
        let mut gate = flow.gate();
        if let Gate::Committed(commit) = std::mem::replace(&mut *gate, Gate::Cancelled) {
            self.undo_commit(&commit);
        }
    }

    fn undo_commit(&self, commit: &Commit) {
        let key = commit.account.key();
        let mut book = self.book_lock();
        let still_ours = book.get(&key) == Some(&commit.account)
            && matches!(self.inner.store.load(&key), Ok(Some(current)) if current == commit.written);
        if !still_ours {
            return;
        }
        match &commit.previous_account {
            Some(previous) => book.add(previous.clone()),
            None => {
                book.remove(&key);
            }
        }
        // Best effort, list first like a disconnect: there is nobody left to
        // report to, and a leftover is at worst an account the user can
        // disconnect.
        let _ = self.save_book(&book);
        let _ = match &commit.previous_credential {
            Some(previous) => self.inner.store.save(&key, previous),
            None => self.inner.store.delete(&key),
        };
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
    /// write the list, record the commit) runs under the flow's gate and the
    /// list lock, so a cancel, a disconnect or another sign-in cannot
    /// interleave with it. If the list cannot be written the token is put
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
        let key = account.key();
        // Lock order everywhere: gate, then list.
        let mut gate = flow.gate();
        if matches!(*gate, Gate::Cancelled) {
            return Err(ForgeError::Cancelled);
        }
        let mut book = self.book_lock();
        let previous_account = book.get(&key).cloned();
        let previous_credential = self.inner.store.load(&key)?;
        self.inner.store.save(&key, &credential)?;
        let before = book.clone();
        book.add(account.clone());
        if let Err(error) = self.save_book(&book) {
            *book = before;
            let token_back = match &previous_credential {
                Some(previous) => self.inner.store.save(&key, previous),
                None => self.inner.store.delete(&key),
            };
            return Err(if token_back.is_err() {
                ForgeError::RollbackFailed
            } else {
                error
            });
        }
        *gate = Gate::Committed(Box::new(Commit {
            account: account.clone(),
            written: credential,
            previous_account,
            previous_credential,
        }));
        Ok(account)
    }

    /// Forgets an account. The list changes first, so a restart cannot bring
    /// back an account whose token is gone; then the token leaves the
    /// keychain. If the keychain refuses, the account is put back (so the
    /// token is not left unseen) and the error is reported.
    pub fn disconnect(&self, key: &str) -> Result<(), ForgeError> {
        let mut book = self.book_lock();
        let before = book.clone();
        if book.remove(key).is_none() {
            return Err(ForgeError::UnknownAccount);
        }
        if let Err(error) = self.save_book(&book) {
            *book = before;
            return Err(error);
        }
        if let Err(error) = self.inner.store.delete(key) {
            *book = before;
            return match self.save_book(&book) {
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
        let book = self.book_lock();
        if book.get(&key).is_none() {
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
        match self.page(&account, &credential.access, cursor) {
            Err(ForgeError::Unauthorized) if credential.refresh.is_some() => {
                let fresh = self.refresh(&account, &credential)?;
                self.page(&account, &fresh.access, cursor)
            }
            other => other,
        }
    }

    fn page(
        &self,
        account: &ForgeAccount,
        token: &Secret,
        cursor: Option<&str>,
    ) -> Result<RepoPage, ForgeError> {
        match account.kind {
            ForgeKind::GitHub => api::github::list_repos_page(
                &self.inner.http,
                &self.inner.config.github.api,
                token,
                cursor,
            ),
            ForgeKind::GitLab => {
                api::gitlab::list_repos_page(&self.inner.http, &account.origin, token, cursor)
            }
        }
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
}

fn load_book(path: &Path) -> Result<AccountBook, ForgeError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(AccountBook::decode(&text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AccountBook::default()),
        Err(_) => Err(ForgeError::Disk(
            "the account list could not be read".into(),
        )),
    }
}

/// Writes the list through a temporary file so a crash never leaves half of
/// it. It holds names and hosts only.
fn save_book(path: &Path, book: &AccountBook) -> Result<(), ForgeError> {
    let disk = |_| ForgeError::Disk("the data folder is not writable".into());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(disk)?;
    }
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, book.encode()).map_err(disk)?;
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
