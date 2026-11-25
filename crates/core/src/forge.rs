//! Forge accounts (GitHub, GitLab, self-hosted GitLab): the pure half.
//!
//! Everything here is data and decisions, with no network, keychain or disk:
//!
//! - which account serves a remote (`AccountBook::resolve`, matching on the
//!   remote's host)
//! - the account list as text (`encode` / `decode`; never a token, only who
//!   is connected where)
//! - the sign-in flow as a small machine (`step_sign_in`), for both the
//!   GitHub device flow and the GitLab browser (PKCE) flow
//! - the repo list the clone dialog filters (`RemoteRepo`, `filter_repos`)
//!
//! Tokens never appear in this module. The shell keeps them in the OS
//! keychain, keyed by [`ForgeAccount::key`]. Every core git feature works
//! with no account at all: nothing in the repo core refers to this module.

use std::collections::BTreeMap;

/// Which API family an account speaks.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ForgeKind {
    GitHub,
    GitLab,
}

impl ForgeKind {
    pub fn label(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "GitHub",
            ForgeKind::GitLab => "GitLab",
        }
    }

    /// The word written in the account file.
    pub fn tag(self) -> &'static str {
        match self {
            ForgeKind::GitHub => "github",
            ForgeKind::GitLab => "gitlab",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "github" => Some(ForgeKind::GitHub),
            "gitlab" => Some(ForgeKind::GitLab),
            _ => None,
        }
    }
}

/// How the account signed in.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AuthMethod {
    OAuth,
    /// A personal access token the user pasted.
    Pat,
}

impl AuthMethod {
    pub fn tag(self) -> &'static str {
        match self {
            AuthMethod::OAuth => "oauth",
            AuthMethod::Pat => "pat",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "oauth" => Some(AuthMethod::OAuth),
            "pat" => Some(AuthMethod::Pat),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Origins: where a forge lives.
// ---------------------------------------------------------------------------

/// Why an instance address was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OriginProblem {
    Empty,
    /// Something other than `http` or `https`.
    Scheme,
    /// `http` to a host that is not this machine: a token would cross the
    /// network in clear text.
    Insecure,
    /// A user name or password in the address.
    UserInfo,
    /// A path, query or fragment. Instances under a sub-path are not
    /// supported yet.
    Path,
    Host,
    Port,
}

/// `scheme://host[:port]`, normalized: lower case, no trailing slash, the
/// default port dropped. The only shape accepted for a forge instance.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Origin {
    text: String,
    host: String,
    /// The port requests go to: the written one, or 80 / 443 by scheme.
    port: u16,
}

impl Origin {
    /// Reads what a user typed (`gitlab.example.com`,
    /// `https://gitlab.example.com/`, `http://localhost:8080`). A bare host
    /// means `https`.
    pub fn parse(input: &str) -> Result<Origin, OriginProblem> {
        let input = input.trim();
        if input.is_empty() {
            return Err(OriginProblem::Empty);
        }
        let (scheme, rest) = match input.split_once("://") {
            Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
            None => ("https".to_owned(), input),
        };
        if scheme != "http" && scheme != "https" {
            return Err(OriginProblem::Scheme);
        }
        let (authority, tail) = match rest.find(['/', '?', '#']) {
            Some(at) => rest.split_at(at),
            None => (rest, ""),
        };
        if !(tail.is_empty() || tail == "/") {
            return Err(OriginProblem::Path);
        }
        if authority.contains('@') {
            return Err(OriginProblem::UserInfo);
        }
        let (host, port) = split_host_port(authority).ok_or(OriginProblem::Host)?;
        let host = host.to_ascii_lowercase();
        if !valid_host(&host) {
            return Err(OriginProblem::Host);
        }
        let port = match port {
            None => None,
            Some(text) => match text.parse::<u16>() {
                Ok(port) if port != 0 => Some(port),
                _ => return Err(OriginProblem::Port),
            },
        };
        if scheme == "http" && !is_loopback(&host) {
            return Err(OriginProblem::Insecure);
        }
        let default = if scheme == "https" { 443 } else { 80 };
        let effective = port.unwrap_or(default);
        let port = port.filter(|port| *port != default);
        let shown = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.clone()
        };
        let text = match port {
            Some(port) => format!("{scheme}://{shown}:{port}"),
            None => format!("{scheme}://{shown}"),
        };
        Ok(Origin {
            text,
            host,
            port: effective,
        })
    }

    /// `https://gitlab.example.com:8443`, no trailing slash.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The host name alone (no port): what a remote's host is compared to.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port requests go to (443 for a bare `https://host`).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether the scheme is https.
    pub fn is_https(&self) -> bool {
        self.text.starts_with("https://")
    }

    pub fn is_secure(&self) -> bool {
        self.text.starts_with("https://") || is_loopback(&self.host)
    }
}

fn split_host_port(authority: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        // Brackets hold an IPv6 literal, nothing else (not a name).
        host.parse::<std::net::Ipv6Addr>().ok()?;
        return match after {
            "" => Some((host, None)),
            _ => Some((host, Some(after.strip_prefix(':')?))),
        };
    }
    // More than one colon without brackets is an IPv6 literal written
    // without them: ambiguous with a port, so refused.
    if authority.matches(':').count() > 1 {
        return None;
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host, Some(port))),
        None => Some((authority, None)),
    }
}

fn valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    if host.contains(':') {
        // An IPv6 literal: it must parse as one.
        return host.parse::<std::net::Ipv6Addr>().is_ok();
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    })
}

fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback())
}

// ---------------------------------------------------------------------------
// Accounts and the book that holds them.
// ---------------------------------------------------------------------------

/// Who is connected where. No secret: the token lives in the keychain under
/// [`ForgeAccount::key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeAccount {
    pub kind: ForgeKind,
    pub origin: Origin,
    pub username: String,
    pub method: AuthMethod,
}

impl ForgeAccount {
    /// `None` when the user name cannot be stored on one line of the
    /// account file (empty, or a control character in it).
    pub fn new(
        kind: ForgeKind,
        origin: Origin,
        username: &str,
        method: AuthMethod,
    ) -> Option<Self> {
        let username = username.trim();
        if username.is_empty() || username.chars().any(char::is_control) {
            return None;
        }
        Some(Self {
            kind,
            origin,
            username: username.to_owned(),
            method,
        })
    }

    /// The account's host name (no port).
    pub fn host(&self) -> &str {
        self.origin.host()
    }

    /// A stable id: the keychain entry name and the key of the host
    /// preference. Two accounts are the same when their keys are.
    pub fn key(&self) -> String {
        format!(
            "{}|{}|{}",
            self.kind.tag(),
            self.origin.as_str(),
            self.username.to_ascii_lowercase()
        )
    }

    /// `name @ host`, for lists.
    pub fn label(&self) -> String {
        format!("{} @ {}", self.username, self.origin.host())
    }
}

/// Where a remote address points: the host, and how git reaches it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteTarget {
    pub host: String,
    pub transport: Transport,
}

/// How git reaches a remote. Only SSH may go through a forge's alternate
/// SSH host (`ssh.github.com`); every other transport names its server
/// exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `http` or `https`: the scheme and port count.
    Web(WebEndpoint),
    /// `ssh://` or scp-style `git@host:path`; the port is the SSH server's.
    Ssh,
    /// `git://` and anything else with a host.
    Other,
}

/// The scheme and port a web request to a remote goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebEndpoint {
    pub https: bool,
    pub port: u16,
}

/// The host (and web endpoint) of a remote address, or `None` for a local path
/// or an address whose authority is malformed. Understands
/// `https://u@host:port/x`, `ssh://git@host/x` and scp-style `git@host:x`.
pub fn remote_target(url: &str) -> Option<RemoteTarget> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let (scheme, authority) = if let Some((scheme, rest)) = url.split_once("://") {
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        (Some(scheme.to_ascii_lowercase()), &rest[..end])
    } else {
        // scp-style: `[user@]host:path`. A local path, or a drive letter
        // (`C:\work`, `c:/work`), has no host.
        if url.starts_with(['/', '.', '\\', '~']) {
            return None;
        }
        let (head, _) = url.split_once(':')?;
        if head.len() == 1 || head.contains(['/', '\\']) {
            return None;
        }
        (None, head)
    };
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port_text) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        // The brackets hold an IPv6 literal, nothing else.
        host.parse::<std::net::Ipv6Addr>().ok()?;
        // Only `]` or `]:port` may follow; anything else is malformed.
        match after {
            "" => (host, None),
            _ => (host, Some(after.strip_prefix(':')?)),
        }
    } else if scheme.is_some() {
        // More than one colon without brackets: an IPv6 literal written
        // without them. Ambiguous with a port, so refused.
        if authority.matches(':').count() > 1 {
            return None;
        }
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    } else {
        (authority, None)
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if !host.is_ascii() || host.is_empty() {
        return None;
    }
    let transport = match scheme.as_deref() {
        None | Some("ssh" | "git+ssh" | "ssh+git") => Transport::Ssh,
        Some(scheme @ ("http" | "https")) => {
            let https = scheme == "https";
            let port = match port_text {
                None | Some("") => {
                    if https {
                        443
                    } else {
                        80
                    }
                }
                // An unreadable port matches no account.
                Some(text) => text.parse::<u16>().unwrap_or(0),
            };
            Transport::Web(WebEndpoint { https, port })
        }
        Some(_) => Transport::Other,
    };
    Some(RemoteTarget { host, transport })
}

/// The host of a remote address, lower case and without port or user, or
/// `None` for a local path.
pub fn host_of_remote(url: &str) -> Option<String> {
    remote_target(url).map(|target| target.host)
}

/// Hosts that serve the same forge as `host` over another door.
fn same_forge(a: &str, b: &str) -> bool {
    fn canonical(host: &str) -> &str {
        match host {
            "ssh.github.com" => "github.com",
            "altssh.gitlab.com" => "gitlab.com",
            other => other,
        }
    }
    canonical(a) == canonical(b)
}

/// Which account to use for a remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// The address has no host (a local path).
    LocalPath,
    /// A host with no connected account: stay anonymous.
    NoAccount {
        host: String,
    },
    Use(ForgeAccount),
    /// Several accounts serve this address and none is preferred: ask.
    Choose(Vec<ForgeAccount>),
}

/// The connected accounts, and which one each host prefers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountBook {
    accounts: Vec<ForgeAccount>,
    /// Host name -> account key.
    preferred: BTreeMap<String, String>,
}

impl AccountBook {
    pub fn accounts(&self) -> &[ForgeAccount] {
        &self.accounts
    }

    pub fn get(&self, key: &str) -> Option<&ForgeAccount> {
        self.accounts.iter().find(|account| account.key() == key)
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Adds an account, or replaces the one with the same key (signing in
    /// again changes the method, not the count).
    pub fn add(&mut self, account: ForgeAccount) {
        let key = account.key();
        match self.accounts.iter_mut().find(|a| a.key() == key) {
            Some(existing) => *existing = account,
            None => self.accounts.push(account),
        }
    }

    /// Removes the account and every preference that pointed at it.
    pub fn remove(&mut self, key: &str) -> Option<ForgeAccount> {
        let at = self.accounts.iter().position(|a| a.key() == key)?;
        self.preferred.retain(|_, preferred| preferred != key);
        Some(self.accounts.remove(at))
    }

    /// Makes `key` the account for its host. Refused when the account does
    /// not exist.
    pub fn prefer(&mut self, key: &str) -> bool {
        let Some(account) = self.get(key) else {
            return false;
        };
        let origin = account.origin.as_str().to_owned();
        self.preferred.insert(origin, key.to_owned());
        true
    }

    /// Whether `key` is the account its host was told to use.
    pub fn is_preferred(&self, key: &str) -> bool {
        self.preferred.values().any(|preferred| preferred == key)
    }

    /// The account that serves `remote_url`.
    pub fn resolve(&self, remote_url: &str) -> Resolution {
        let Some(RemoteTarget { host, transport }) = remote_target(remote_url) else {
            return Resolution::LocalPath;
        };
        let matching: Vec<&ForgeAccount> = self
            .accounts
            .iter()
            .filter(|account| {
                match transport {
                    // A web address names the server itself: no aliases.
                    Transport::Web(web) => {
                        account.host() == host
                            && web.https == account.origin.is_https()
                            && web.port == account.origin.port()
                    }
                    // The SSH-over-443 hosts are doors to the same forge.
                    Transport::Ssh => same_forge(account.host(), &host),
                    // Any other transport names its server exactly.
                    Transport::Other => account.host() == host,
                }
            })
            .collect();
        match matching.as_slice() {
            [] => Resolution::NoAccount { host },
            [only] => Resolution::Use((*only).clone()),
            many => {
                let preferred = many.iter().find(|account| {
                    self.preferred.get(account.origin.as_str()) == Some(&account.key())
                });
                match preferred {
                    Some(account) => Resolution::Use((**account).clone()),
                    None => Resolution::Choose(many.iter().map(|a| (*a).clone()).collect()),
                }
            }
        }
    }

    // -- text form ----------------------------------------------------------

    const HEADER: &'static str = "invoke-git forge accounts v1";

    /// The book as text: who is connected where. Never a token.
    pub fn encode(&self) -> String {
        let mut text = format!("{}\n", Self::HEADER);
        for account in &self.accounts {
            text.push_str(&format!(
                "account\t{}\t{}\t{}\t{}\n",
                account.kind.tag(),
                account.method.tag(),
                account.origin.as_str(),
                account.username
            ));
        }
        for (host, key) in &self.preferred {
            text.push_str(&format!("prefer\t{host}\t{key}\n"));
        }
        text
    }

    /// Reads what [`encode`](Self::encode) wrote. Lines it cannot read are
    /// dropped: a damaged file must never stop the app from starting.
    pub fn decode(text: &str) -> AccountBook {
        let mut book = AccountBook::default();
        let mut lines = text.lines();
        if lines.next() != Some(Self::HEADER) {
            return book;
        }
        let mut preferences = Vec::new();
        for line in lines {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                ["account", kind, method, origin, username] => {
                    let account = ForgeKind::from_tag(kind)
                        .zip(AuthMethod::from_tag(method))
                        .zip(Origin::parse(origin).ok())
                        .and_then(|((kind, method), origin)| {
                            ForgeAccount::new(kind, origin, username, method)
                        });
                    if let Some(account) = account {
                        book.add(account);
                    }
                }
                ["prefer", _host, key] => preferences.push(*key),
                _ => {}
            }
        }
        for key in preferences {
            book.prefer(key);
        }
        book
    }
}

// ---------------------------------------------------------------------------
// Sign-in: one machine for the device flow and the browser flow.
// ---------------------------------------------------------------------------

/// One sign-in attempt. The UI numbers its attempts; every answer the shell
/// sends back names the attempt it belongs to, so an answer that arrives
/// after its attempt was cancelled or replaced is recognised and dropped.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Attempt(pub u64);

/// How the user proves who they are.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SignInMethod {
    /// GitHub: a code to type on a web page; the app polls until it is used.
    GitHubDevice,
    /// GitLab: a browser tab that redirects back to this machine (PKCE).
    GitLabBrowser,
}

/// What the user has to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserPrompt {
    /// Open `verification_url` and type `user_code`.
    DeviceCode {
        user_code: String,
        verification_url: String,
    },
    /// Open `url` and approve.
    Browser { url: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInFailure {
    /// The user said no on the forge's page.
    Denied,
    /// The code or the page timed out.
    Expired,
    /// The user cancelled this attempt. Its number is kept: the shell may
    /// still report that undoing it failed.
    Cancelled(Attempt),
    /// Anything else, as a sentence that holds no secret.
    Error(String),
    /// Cancelled, but undoing what the flow had already stored failed: a
    /// token or an account may be left behind. The sentence says where.
    CancelledWithLeftover(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInPhase {
    Idle,
    /// Asking the forge for a code (or starting the browser flow).
    Requesting {
        method: SignInMethod,
        attempt: Attempt,
    },
    AwaitingUser {
        method: SignInMethod,
        prompt: UserPrompt,
        /// Seconds between polls (device flow only).
        interval_secs: u32,
        attempt: Attempt,
    },
    /// Signed in. The token is already in the keychain.
    Authorized(ForgeAccount),
    Failed(SignInFailure),
}

impl SignInPhase {
    pub fn is_active(&self) -> bool {
        self.attempt().is_some()
    }

    /// The attempt in progress, if one is.
    pub fn attempt(&self) -> Option<Attempt> {
        match self {
            SignInPhase::Requesting { attempt, .. } | SignInPhase::AwaitingUser { attempt, .. } => {
                Some(*attempt)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInEvent {
    // From the UI.
    Begin {
        method: SignInMethod,
        attempt: Attempt,
    },
    Cancel,
    /// From the shell, about one attempt.
    Reply {
        attempt: Attempt,
        reply: SignInReply,
    },
}

/// What the shell reports about an attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInReply {
    PromptReady {
        prompt: UserPrompt,
        interval_secs: u32,
    },
    /// The user has not finished yet.
    Pending,
    /// The forge asks for slower polling.
    SlowDown,
    Granted(ForgeAccount),
    Denied,
    Expired,
    Errored(String),
    /// After a cancel: the undo of an already stored account succeeded.
    CleanedUp,
    /// After a cancel: the undo of an already stored account failed.
    CleanupFailed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInCommand {
    /// Ask the forge for a code, or start the browser flow, for `attempt`.
    Request {
        method: SignInMethod,
        attempt: Attempt,
    },
    /// Ask the forge once more, after this many seconds.
    PollAfter { secs: u32 },
    /// Stop whatever is running (poll timer, local listener).
    Abort,
}

/// Seconds between polls: at least 1, at most `MAX_INTERVAL`.
pub const MAX_INTERVAL: u32 = 300;
/// GitHub's rule for `slow_down`: five more seconds.
pub const SLOW_DOWN_STEP: u32 = 5;

/// The device flow polls on a timer; the browser flow waits for the redirect.
fn poll(method: SignInMethod, secs: u32) -> Vec<SignInCommand> {
    match method {
        SignInMethod::GitHubDevice => vec![SignInCommand::PollAfter { secs }],
        SignInMethod::GitLabBrowser => vec![],
    }
}

pub fn step_sign_in(phase: SignInPhase, event: SignInEvent) -> (SignInPhase, Vec<SignInCommand>) {
    use SignInCommand as C;
    use SignInEvent as E;
    use SignInPhase as P;
    use SignInReply as R;
    match (phase, event) {
        (P::Idle | P::Authorized(_) | P::Failed(_), E::Begin { method, attempt }) => (
            P::Requesting { method, attempt },
            vec![C::Request { method, attempt }],
        ),
        (phase, E::Cancel) => match phase.attempt() {
            Some(attempt) => (P::Failed(SignInFailure::Cancelled(attempt)), vec![C::Abort]),
            None => (phase, vec![]),
        },
        // The only answer a finished attempt still takes: its undo failed.
        (P::Failed(SignInFailure::Cancelled(cancelled)), E::Reply { attempt, reply })
            if attempt == cancelled =>
        {
            match reply {
                R::CleanupFailed(message) => (
                    P::Failed(SignInFailure::CancelledWithLeftover(message)),
                    vec![],
                ),
                _ => (P::Failed(SignInFailure::Cancelled(cancelled)), vec![]),
            }
        }
        // An answer for the attempt in progress.
        (phase, E::Reply { attempt, reply }) if phase.attempt() == Some(attempt) => {
            answer(phase, attempt, reply)
        }
        // Anything else is late or stray (an answer for an older attempt, a
        // second Begin): nothing changes.
        (phase, _) => (phase, vec![]),
    }
}

/// An answer for the attempt in progress.
fn answer(
    phase: SignInPhase,
    attempt: Attempt,
    reply: SignInReply,
) -> (SignInPhase, Vec<SignInCommand>) {
    use SignInPhase as P;
    use SignInReply as R;
    match (phase, reply) {
        (
            P::Requesting { method, .. },
            R::PromptReady {
                prompt,
                interval_secs,
            },
        ) => {
            let interval_secs = interval_secs.clamp(1, MAX_INTERVAL);
            (
                P::AwaitingUser {
                    method,
                    prompt,
                    interval_secs,
                    attempt,
                },
                poll(method, interval_secs),
            )
        }
        (
            P::AwaitingUser {
                method,
                prompt,
                interval_secs,
                ..
            },
            R::Pending,
        ) => (
            P::AwaitingUser {
                method,
                prompt,
                interval_secs,
                attempt,
            },
            poll(method, interval_secs),
        ),
        (
            P::AwaitingUser {
                method,
                prompt,
                interval_secs,
                ..
            },
            R::SlowDown,
        ) => {
            let interval_secs = (interval_secs + SLOW_DOWN_STEP).min(MAX_INTERVAL);
            (
                P::AwaitingUser {
                    method,
                    prompt,
                    interval_secs,
                    attempt,
                },
                poll(method, interval_secs),
            )
        }
        (_, R::Granted(account)) => (P::Authorized(account), vec![]),
        (_, R::Denied) => (P::Failed(SignInFailure::Denied), vec![]),
        (_, R::Expired) => (P::Failed(SignInFailure::Expired), vec![]),
        (_, R::Errored(message)) => (P::Failed(SignInFailure::Error(message)), vec![]),
        // Out of order for this phase (a poll answer before the prompt, a
        // cleanup report while running): nothing changes.
        (phase, _) => (phase, vec![]),
    }
}

// ---------------------------------------------------------------------------
// Repo list.
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
    /// GitLab: visible to signed-in users of the instance.
    Internal,
}

/// One repository a forge lists for an account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRepo {
    /// `owner/name` (GitLab: the full path with groups).
    pub full_name: String,
    /// The HTTPS address git clones from.
    pub clone_url: String,
    pub description: String,
    pub visibility: Visibility,
    pub default_branch: Option<String>,
    /// As the forge wrote it (ISO 8601); sorts correctly as text.
    pub updated_at: Option<String>,
}

/// The positions of the repos that match `query`: every word of the query
/// must appear in the name or the description, ignoring case. An empty query
/// matches everything.
pub fn filter_repos(repos: &[RemoteRepo], query: &str) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    repos
        .iter()
        .enumerate()
        .filter(|(_, repo)| {
            let haystack = format!("{} {}", repo.full_name, repo.description).to_lowercase();
            words.iter().all(|word| haystack.contains(word))
        })
        .map(|(at, _)| at)
        .collect()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn origin(text: &str) -> Origin {
        Origin::parse(text).unwrap()
    }

    fn account(kind: ForgeKind, host: &str, user: &str) -> ForgeAccount {
        ForgeAccount::new(kind, origin(host), user, AuthMethod::Pat).unwrap()
    }

    #[test]
    fn origins_are_normalized() {
        for (input, text, host) in [
            ("gitlab.com", "https://gitlab.com", "gitlab.com"),
            (
                "  GitLab.Example.COM/ ",
                "https://gitlab.example.com",
                "gitlab.example.com",
            ),
            ("https://git.corp:8443", "https://git.corp:8443", "git.corp"),
            ("https://git.corp:443/", "https://git.corp", "git.corp"),
            (
                "http://localhost:8080",
                "http://localhost:8080",
                "localhost",
            ),
            ("http://127.0.0.1:9", "http://127.0.0.1:9", "127.0.0.1"),
            ("http://[::1]:9", "http://[::1]:9", "::1"),
        ] {
            let parsed = origin(input);
            assert_eq!(parsed.as_str(), text, "{input}");
            assert_eq!(parsed.host(), host, "{input}");
        }
    }

    #[test]
    fn origins_that_could_leak_a_token_or_confuse_a_host_are_refused() {
        for (input, problem) in [
            ("", OriginProblem::Empty),
            ("   ", OriginProblem::Empty),
            ("ftp://gitlab.com", OriginProblem::Scheme),
            ("http://gitlab.example.com", OriginProblem::Insecure),
            ("http://10.0.0.5", OriginProblem::Insecure),
            ("http://127.0.0.1.evil.example", OriginProblem::Insecure),
            ("https://user:pw@gitlab.com", OriginProblem::UserInfo),
            ("https://gitlab.com/sub/path", OriginProblem::Path),
            ("https://gitlab.com?x=1", OriginProblem::Path),
            ("https://gitlab.com#frag", OriginProblem::Path),
            ("https://", OriginProblem::Host),
            ("https://bad host", OriginProblem::Host),
            ("https://-x.com", OriginProblem::Host),
            ("https://gitlab.com:0", OriginProblem::Port),
            ("https://gitlab.com:99999", OriginProblem::Port),
            ("https://gitlab.com:abc", OriginProblem::Port),
        ] {
            assert_eq!(Origin::parse(input), Err(problem), "{input:?}");
        }
    }

    #[test]
    fn the_host_of_a_remote_is_found_in_every_address_style() {
        for (url, host) in [
            ("https://github.com/o/r.git", Some("github.com")),
            ("https://User@GitHub.com/o/r", Some("github.com")),
            ("https://user:secret@git.corp:8443/o/r", Some("git.corp")),
            ("ssh://git@gitlab.com:22/o/r.git", Some("gitlab.com")),
            ("git@github.com:o/r.git", Some("github.com")),
            ("github.com:o/r.git", Some("github.com")),
            ("ssh://git@[::1]:2222/o/r", Some("::1")),
            ("HTTPS://GITHUB.COM./o/r", Some("github.com")),
            (r"C:\work\repo", None),
            ("c:/work/repo", None),
            ("/srv/git/repo.git", None),
            ("./repo", None),
            (r"\\server\share\repo", None),
            ("repo", None),
            ("", None),
        ] {
            assert_eq!(host_of_remote(url).as_deref(), host, "{url}");
        }
    }

    #[test]
    fn a_remote_uses_the_account_of_its_host() {
        let mut book = AccountBook::default();
        book.add(account(ForgeKind::GitHub, "github.com", "ana"));
        book.add(account(ForgeKind::GitLab, "gitlab.com", "ana"));
        book.add(account(ForgeKind::GitLab, "git.corp", "ana-work"));

        let github = match book.resolve("git@github.com:o/r.git") {
            Resolution::Use(account) => account,
            other => panic!("{other:?}"),
        };
        assert_eq!(github.kind, ForgeKind::GitHub);
        // The SSH-over-443 hosts belong to the same forges.
        assert!(matches!(
            book.resolve("ssh://git@ssh.github.com:443/o/r.git"),
            Resolution::Use(a) if a.host() == "github.com"
        ));
        assert!(matches!(
            book.resolve("ssh://git@altssh.gitlab.com:443/o/r.git"),
            Resolution::Use(a) if a.host() == "gitlab.com"
        ));
        assert!(matches!(
            book.resolve("https://git.corp/team/tool"),
            Resolution::Use(a) if a.username == "ana-work"
        ));
        assert_eq!(
            book.resolve("https://bitbucket.org/o/r"),
            Resolution::NoAccount {
                host: "bitbucket.org".into()
            }
        );
        assert_eq!(book.resolve(r"C:\work\repo"), Resolution::LocalPath);
        // A look-alike host is not the forge.
        assert!(matches!(
            book.resolve("https://github.com.evil.example/o/r"),
            Resolution::NoAccount { .. }
        ));
        assert!(matches!(
            book.resolve("https://notgithub.com/o/r"),
            Resolution::NoAccount { .. }
        ));
    }

    #[test]
    fn a_web_remote_must_name_the_port_of_the_account() {
        let mut book = AccountBook::default();
        book.add(account(ForgeKind::GitLab, "https://git.corp:8443", "ana"));
        // The port is part of where the account's token is valid.
        for url in [
            "https://git.corp:8443/team/tool.git",
            "https://user@git.corp:8443/team/tool.git",
            // SSH and scp-style remotes carry the SSH server's port, not
            // the web port, so only the host counts.
            "ssh://git@git.corp:22/team/tool.git",
            "git@git.corp:team/tool.git",
        ] {
            assert!(matches!(book.resolve(url), Resolution::Use(_)), "{url}");
        }
        for url in [
            "https://git.corp/team/tool.git",
            "https://git.corp:9000/team/tool.git",
            "https://git.corp:/team/tool.git",
            "https://git.corp:notaport/team/tool.git",
            "http://git.corp:80/team/tool.git",
        ] {
            assert!(
                matches!(book.resolve(url), Resolution::NoAccount { .. }),
                "{url}"
            );
        }
        // The default port may be written or left out.
        let mut plain = AccountBook::default();
        plain.add(account(ForgeKind::GitHub, "github.com", "ana"));
        for url in ["https://github.com/o/r", "https://github.com:443/o/r"] {
            assert!(matches!(plain.resolve(url), Resolution::Use(_)), "{url}");
        }
        assert!(matches!(
            plain.resolve("https://github.com:8443/o/r"),
            Resolution::NoAccount { .. }
        ));
        // Two ports on one host are two accounts, each with its own choice.
        book.add(account(ForgeKind::GitLab, "https://git.corp", "ana"));
        assert!(matches!(
            book.resolve("https://git.corp/x.git"),
            Resolution::Use(a) if a.origin.as_str() == "https://git.corp"
        ));
        assert!(matches!(
            book.resolve("git@git.corp:x.git"),
            Resolution::Choose(list) if list.len() == 2
        ));
    }

    #[test]
    fn a_web_remote_must_also_name_the_scheme_of_the_account() {
        let mut book = AccountBook::default();
        book.add(account(ForgeKind::GitLab, "https://git.corp", "ana"));
        // Port 443 over plain http is another service, not the https account.
        for url in [
            "http://git.corp:443/o/r",
            "http://git.corp/o/r",
            "http://git.corp:80/o/r",
        ] {
            assert!(
                matches!(book.resolve(url), Resolution::NoAccount { .. }),
                "{url}"
            );
        }
        assert!(matches!(
            book.resolve("https://git.corp:443/o/r"),
            Resolution::Use(_)
        ));
        let mut local = AccountBook::default();
        local.add(account(ForgeKind::GitLab, "http://localhost:8080", "ana"));
        assert!(matches!(
            local.resolve("http://localhost:8080/o/r"),
            Resolution::Use(_)
        ));
        assert!(matches!(
            local.resolve("https://localhost:8080/o/r"),
            Resolution::NoAccount { .. }
        ));
    }

    #[test]
    fn malformed_authorities_never_resolve_to_an_account() {
        let mut book = AccountBook::default();
        book.add(account(ForgeKind::GitLab, "https://[::1]:8443", "ana"));
        book.add(account(ForgeKind::GitLab, "https://git.corp", "ana"));
        assert!(matches!(
            book.resolve("https://[::1]:8443/o/r"),
            Resolution::Use(_)
        ));
        for url in [
            // Junk after the closing bracket.
            "https://[::1]8443/o/r",
            "https://[::1]junk:8443/o/r",
            // IPv6 without brackets is ambiguous with a port.
            "https://::1/o/r",
            "https://::1:8443/o/r",
            "https://a:b:c/o/r",
        ] {
            assert_eq!(book.resolve(url), Resolution::LocalPath, "{url}");
        }
        for text in ["https://::1", "http://::1:8080", "https://a:b:c"] {
            assert_eq!(Origin::parse(text), Err(OriginProblem::Host), "{text}");
        }
        assert_eq!(
            Origin::parse("https://[::1]").unwrap().as_str(),
            "https://[::1]"
        );
        assert_eq!(Origin::parse("https://[::1]").unwrap().port(), 443);
    }

    #[test]
    fn ssh_aliases_never_stand_in_for_a_web_address() {
        let mut book = AccountBook::default();
        book.add(account(ForgeKind::GitHub, "github.com", "ana"));
        book.add(account(ForgeKind::GitLab, "gitlab.com", "ana"));
        for url in [
            "ssh://git@ssh.github.com:443/o/r.git",
            "git@ssh.github.com:o/r.git",
            "ssh://git@altssh.gitlab.com:443/o/r.git",
        ] {
            assert!(matches!(book.resolve(url), Resolution::Use(_)), "{url}");
        }
        // The same names over https are other servers, not the forge.
        for url in [
            "https://ssh.github.com/o/r.git",
            "https://ssh.github.com:443/o/r.git",
            "https://altssh.gitlab.com/o/r.git",
        ] {
            assert!(
                matches!(book.resolve(url), Resolution::NoAccount { .. }),
                "{url}"
            );
        }
    }

    #[test]
    fn only_ssh_remotes_may_use_the_ssh_alias_hosts() {
        let mut book = AccountBook::default();
        book.add(account(ForgeKind::GitHub, "github.com", "ana"));
        for url in [
            "ssh://git@ssh.github.com:443/o/r.git",
            "git+ssh://git@ssh.github.com/o/r.git",
            "git@ssh.github.com:o/r.git",
            "git://github.com/o/r.git",
        ] {
            assert!(matches!(book.resolve(url), Resolution::Use(_)), "{url}");
        }
        for url in [
            "git://ssh.github.com/o/r.git",
            "ftp://ssh.github.com/o/r.git",
            "git://notgithub.com/o/r.git",
        ] {
            assert!(
                matches!(book.resolve(url), Resolution::NoAccount { .. }),
                "{url}"
            );
        }
    }

    #[test]
    fn a_bracketed_host_must_be_an_ipv6_literal() {
        let mut book = AccountBook::default();
        book.add(account(ForgeKind::GitLab, "https://[::1]:8443", "ana"));
        assert!(matches!(
            book.resolve("https://[::1]:8443/o/r"),
            Resolution::Use(_)
        ));
        for url in [
            "https://[git.corp]:8443/o/r",
            "https://[abc]/o/r",
            "https://[::1::1]:8443/o/r",
            "https://[:::]/o/r",
            "https://[]/o/r",
            "https://[zz::1]/o/r",
            "ssh://git@[git.corp]/o/r",
        ] {
            assert_eq!(book.resolve(url), Resolution::LocalPath, "{url}");
        }
        for text in [
            "https://[git.corp]",
            "https://[::1::1]",
            "https://[abc]",
            "https://[]",
        ] {
            assert_eq!(Origin::parse(text), Err(OriginProblem::Host), "{text}");
        }
        assert!(Origin::parse("https://[2001:db8::1]:8443").is_ok());
    }

    #[test]
    fn two_accounts_on_one_host_ask_until_one_is_preferred() {
        let mut book = AccountBook::default();
        let personal = account(ForgeKind::GitHub, "github.com", "ana");
        let work = account(ForgeKind::GitHub, "github.com", "ana-corp");
        book.add(personal.clone());
        book.add(work.clone());
        assert_eq!(
            book.resolve("https://github.com/o/r"),
            Resolution::Choose(vec![personal.clone(), work.clone()])
        );
        assert!(book.prefer(&work.key()));
        assert_eq!(
            book.resolve("https://github.com/o/r"),
            Resolution::Use(work.clone())
        );
        // Removing the preferred account asks again, and forgets the choice.
        book.add(account(ForgeKind::GitHub, "github.com", "ana-third"));
        assert!(book.remove(&work.key()).is_some());
        assert!(matches!(
            book.resolve("https://github.com/o/r"),
            Resolution::Choose(list) if list.len() == 2
        ));
        // A key that is not in the book cannot be preferred.
        assert!(!book.prefer("github|https://github.com|ghost"));
    }

    #[test]
    fn signing_in_twice_replaces_instead_of_duplicating() {
        let mut book = AccountBook::default();
        book.add(account(ForgeKind::GitHub, "github.com", "Ana"));
        let mut again = account(ForgeKind::GitHub, "github.com", "ana");
        again.method = AuthMethod::OAuth;
        book.add(again);
        assert_eq!(book.accounts().len(), 1);
        assert_eq!(book.accounts()[0].method, AuthMethod::OAuth);
    }

    #[test]
    fn the_book_survives_a_round_trip_and_damage() {
        let mut book = AccountBook::default();
        book.add(account(ForgeKind::GitHub, "github.com", "ana"));
        book.add(account(ForgeKind::GitHub, "github.com", "bo"));
        book.add(account(ForgeKind::GitLab, "https://git.corp:8443", "cy"));
        let key = book.accounts()[1].key();
        assert!(book.prefer(&key));
        let text = book.encode();
        assert_eq!(AccountBook::decode(&text), book);
        // No secret-looking field exists in the file.
        assert!(!text.to_lowercase().contains("token"));

        assert_eq!(AccountBook::decode(""), AccountBook::default());
        assert_eq!(AccountBook::decode("garbage\n"), AccountBook::default());
        let damaged = "invoke-git forge accounts v1\naccount\tgithub\n\
             account\tnope\tpat\thttps://x.com\tu\n\
             account\tgithub\tpat\thttp://insecure.example\tu\n\
             prefer\tgithub.com\tgithub|https://github.com|ghost\n\
             account\tgithub\tpat\thttps://github.com\tok\n";
        let book = AccountBook::decode(damaged);
        assert_eq!(book.accounts().len(), 1);
        assert_eq!(book.accounts()[0].username, "ok");
    }

    #[test]
    fn user_names_that_cannot_be_stored_are_refused() {
        for bad in ["", "  ", "a\tb", "a\nb", "a\u{7}b"] {
            assert!(
                ForgeAccount::new(
                    ForgeKind::GitHub,
                    origin("github.com"),
                    bad,
                    AuthMethod::Pat
                )
                .is_none(),
                "{bad:?}"
            );
        }
    }

    fn github_account() -> ForgeAccount {
        account(ForgeKind::GitHub, "github.com", "ana")
    }

    fn prompt() -> UserPrompt {
        UserPrompt::DeviceCode {
            user_code: "ABCD-1234".into(),
            verification_url: "https://github.com/login/device".into(),
        }
    }

    const FIRST: Attempt = Attempt(1);
    const SECOND: Attempt = Attempt(2);

    fn begin(method: SignInMethod, attempt: Attempt) -> SignInEvent {
        SignInEvent::Begin { method, attempt }
    }

    fn reply(attempt: Attempt, reply: SignInReply) -> SignInEvent {
        SignInEvent::Reply { attempt, reply }
    }

    fn ready(attempt: Attempt, interval_secs: u32) -> SignInEvent {
        reply(
            attempt,
            SignInReply::PromptReady {
                prompt: prompt(),
                interval_secs,
            },
        )
    }

    #[test]
    fn the_device_flow_polls_until_the_user_is_done() {
        let (phase, commands) =
            step_sign_in(SignInPhase::Idle, begin(SignInMethod::GitHubDevice, FIRST));
        assert_eq!(
            phase,
            SignInPhase::Requesting {
                method: SignInMethod::GitHubDevice,
                attempt: FIRST
            }
        );
        assert_eq!(
            commands,
            vec![SignInCommand::Request {
                method: SignInMethod::GitHubDevice,
                attempt: FIRST
            }]
        );
        let (phase, commands) = step_sign_in(phase, ready(FIRST, 5));
        assert_eq!(commands, vec![SignInCommand::PollAfter { secs: 5 }]);
        let (phase, commands) = step_sign_in(phase, reply(FIRST, SignInReply::Pending));
        assert_eq!(commands, vec![SignInCommand::PollAfter { secs: 5 }]);
        let (phase, commands) = step_sign_in(phase, reply(FIRST, SignInReply::SlowDown));
        assert_eq!(commands, vec![SignInCommand::PollAfter { secs: 10 }]);
        let (phase, commands) =
            step_sign_in(phase, reply(FIRST, SignInReply::Granted(github_account())));
        assert_eq!(phase, SignInPhase::Authorized(github_account()));
        assert!(commands.is_empty());
    }

    #[test]
    fn the_browser_flow_waits_without_polling() {
        let (phase, _) = step_sign_in(SignInPhase::Idle, begin(SignInMethod::GitLabBrowser, FIRST));
        let (phase, commands) = step_sign_in(
            phase,
            reply(
                FIRST,
                SignInReply::PromptReady {
                    prompt: UserPrompt::Browser {
                        url: "https://gitlab.com/oauth/authorize?x=1".into(),
                    },
                    interval_secs: 0,
                },
            ),
        );
        assert!(commands.is_empty());
        assert!(matches!(phase, SignInPhase::AwaitingUser { .. }));
    }

    #[test]
    fn cancel_stops_the_flow_and_late_answers_change_nothing() {
        let (phase, _) = step_sign_in(SignInPhase::Idle, begin(SignInMethod::GitHubDevice, FIRST));
        let (phase, commands) = step_sign_in(phase, SignInEvent::Cancel);
        assert_eq!(phase, SignInPhase::Failed(SignInFailure::Cancelled(FIRST)));
        assert_eq!(commands, vec![SignInCommand::Abort]);
        // A poll answer that was already in flight must not sign in.
        let (after, commands) = step_sign_in(
            phase.clone(),
            reply(FIRST, SignInReply::Granted(github_account())),
        );
        assert_eq!(after, phase);
        assert!(commands.is_empty());
        // Nothing to cancel when idle.
        let (after, commands) = step_sign_in(SignInPhase::Idle, SignInEvent::Cancel);
        assert_eq!(after, SignInPhase::Idle);
        assert!(commands.is_empty());
    }

    #[test]
    fn an_answer_for_an_older_attempt_never_reaches_the_new_one() {
        // The first attempt was cancelled; its answers were still queued
        // when the second began.
        let (phase, _) = step_sign_in(SignInPhase::Idle, begin(SignInMethod::GitHubDevice, FIRST));
        let (phase, _) = step_sign_in(phase, SignInEvent::Cancel);
        let (phase, _) = step_sign_in(phase, begin(SignInMethod::GitHubDevice, SECOND));
        let (phase, _) = step_sign_in(phase, ready(SECOND, 5));
        for stale in [
            SignInReply::Granted(github_account()),
            SignInReply::Denied,
            SignInReply::Expired,
            SignInReply::Errored("old".into()),
            SignInReply::Pending,
            SignInReply::SlowDown,
            SignInReply::PromptReady {
                prompt: prompt(),
                interval_secs: 1,
            },
            SignInReply::CleanupFailed("old".into()),
        ] {
            let (after, commands) = step_sign_in(phase.clone(), reply(FIRST, stale.clone()));
            assert_eq!(after, phase, "{stale:?}");
            assert!(commands.is_empty(), "{stale:?}");
        }
        // The new attempt's own answer still counts.
        let (after, _) = step_sign_in(phase, reply(SECOND, SignInReply::Granted(github_account())));
        assert_eq!(after, SignInPhase::Authorized(github_account()));
    }

    #[test]
    fn refusals_and_errors_end_the_flow_and_it_can_start_again() {
        for (answer, failure) in [
            (SignInReply::Denied, SignInFailure::Denied),
            (SignInReply::Expired, SignInFailure::Expired),
            (
                SignInReply::Errored("no network".into()),
                SignInFailure::Error("no network".into()),
            ),
        ] {
            let (phase, _) =
                step_sign_in(SignInPhase::Idle, begin(SignInMethod::GitHubDevice, FIRST));
            let (phase, commands) = step_sign_in(phase, reply(FIRST, answer));
            assert_eq!(phase, SignInPhase::Failed(failure));
            assert!(commands.is_empty());
            let (phase, commands) = step_sign_in(phase, begin(SignInMethod::GitLabBrowser, SECOND));
            assert_eq!(
                phase,
                SignInPhase::Requesting {
                    method: SignInMethod::GitLabBrowser,
                    attempt: SECOND
                }
            );
            assert_eq!(commands.len(), 1);
        }
        // A second Begin while one is running is ignored.
        let (phase, _) = step_sign_in(SignInPhase::Idle, begin(SignInMethod::GitHubDevice, FIRST));
        let (after, commands) =
            step_sign_in(phase.clone(), begin(SignInMethod::GitLabBrowser, SECOND));
        assert_eq!(after, phase);
        assert!(commands.is_empty());
    }

    #[test]
    fn a_failed_undo_after_cancel_is_told_and_nothing_else_changes_a_finished_flow() {
        let (phase, _) = step_sign_in(SignInPhase::Idle, begin(SignInMethod::GitHubDevice, FIRST));
        let (phase, _) = step_sign_in(phase, SignInEvent::Cancel);
        // A report for another attempt is not this one's.
        let (same, _) = step_sign_in(
            phase.clone(),
            reply(SECOND, SignInReply::CleanupFailed("other".into())),
        );
        assert_eq!(same, phase);
        let (phase, commands) = step_sign_in(
            phase,
            reply(FIRST, SignInReply::CleanupFailed("remove the entry".into())),
        );
        assert_eq!(
            phase,
            SignInPhase::Failed(SignInFailure::CancelledWithLeftover(
                "remove the entry".into()
            ))
        );
        assert!(commands.is_empty());
        // It is told once, and only for a cancel.
        let (again, _) = step_sign_in(
            phase.clone(),
            reply(FIRST, SignInReply::CleanupFailed("x".into())),
        );
        assert_eq!(again, phase);
        for other in [
            SignInPhase::Idle,
            SignInPhase::Authorized(github_account()),
            SignInPhase::Failed(SignInFailure::Denied),
            SignInPhase::Requesting {
                method: SignInMethod::GitHubDevice,
                attempt: FIRST,
            },
        ] {
            let (after, _) = step_sign_in(
                other.clone(),
                reply(FIRST, SignInReply::CleanupFailed("x".into())),
            );
            assert_eq!(after, other);
        }
    }

    fn answer() -> impl Strategy<Value = SignInReply> {
        prop_oneof![
            (0u32..1000).prop_map(|interval_secs| SignInReply::PromptReady {
                prompt: prompt(),
                interval_secs
            }),
            Just(SignInReply::Pending),
            Just(SignInReply::SlowDown),
            Just(SignInReply::Granted(github_account())),
            Just(SignInReply::Denied),
            Just(SignInReply::Expired),
            Just(SignInReply::Errored("x".into())),
            Just(SignInReply::CleanupFailed("x".into())),
        ]
    }

    fn event() -> impl Strategy<Value = SignInEvent> {
        let attempt = (0u64..4).prop_map(Attempt);
        prop_oneof![
            attempt.clone().prop_map(|attempt| SignInEvent::Begin {
                method: SignInMethod::GitHubDevice,
                attempt
            }),
            attempt.clone().prop_map(|attempt| SignInEvent::Begin {
                method: SignInMethod::GitLabBrowser,
                attempt
            }),
            Just(SignInEvent::Cancel),
            (attempt, answer()).prop_map(|(attempt, reply)| SignInEvent::Reply { attempt, reply }),
        ]
    }

    proptest! {
        /// Whatever order events arrive in: an answer only ever moves the
        /// attempt it names, a finished flow only restarts through `Begin`, a
        /// poll is never asked from a phase that is not waiting, and the
        /// interval stays within its bounds.
        #[test]
        fn the_sign_in_machine_keeps_its_promises(events in proptest::collection::vec(event(), 0..40)) {
            let mut phase = SignInPhase::Idle;
            for event in events {
                let before = phase.clone();
                let (next, commands) = step_sign_in(phase, event.clone());
                if let SignInEvent::Reply { attempt, reply } = &event {
                    let current = before.attempt() == Some(*attempt);
                    // The one thing a finished attempt still accepts: news
                    // that its undo after a cancel failed.
                    let leftover = matches!(reply, SignInReply::CleanupFailed(_))
                        && before == SignInPhase::Failed(SignInFailure::Cancelled(*attempt));
                    if !current && !leftover {
                        prop_assert_eq!(&next, &before);
                        prop_assert!(commands.is_empty());
                    }
                }
                if matches!(before, SignInPhase::Authorized(_) | SignInPhase::Failed(_))
                    && !matches!(event, SignInEvent::Begin { .. } | SignInEvent::Reply { .. })
                {
                    prop_assert_eq!(&next, &before);
                    prop_assert!(commands.is_empty());
                }
                for command in &commands {
                    if let SignInCommand::PollAfter { secs } = command {
                        let polling = matches!(&next, SignInPhase::AwaitingUser { method: SignInMethod::GitHubDevice, .. });
                        prop_assert!(polling);
                        prop_assert!((1..=MAX_INTERVAL).contains(secs));
                    }
                }
                if let SignInPhase::AwaitingUser { interval_secs, .. } = &next {
                    prop_assert!((1..=MAX_INTERVAL).contains(interval_secs));
                }
                phase = next;
            }
        }


        #[test]
        fn a_book_never_holds_two_accounts_with_one_key(
            users in proptest::collection::vec("[a-cA-C]{1,2}", 0..12),
        ) {
            let mut book = AccountBook::default();
            for user in &users {
                book.add(account(ForgeKind::GitHub, "github.com", user));
            }
            let mut keys: Vec<String> = book.accounts().iter().map(ForgeAccount::key).collect();
            let all = keys.len();
            keys.sort();
            keys.dedup();
            prop_assert_eq!(keys.len(), all);
            // And the text form always reads back to the same book.
            prop_assert_eq!(AccountBook::decode(&book.encode()), book);
        }

        #[test]
        fn the_host_of_a_remote_never_panics_and_is_lower_case(url in "\\PC{0,60}") {
            if let Some(host) = host_of_remote(&url) {
                prop_assert!(!host.is_empty());
                prop_assert_eq!(host.clone(), host.to_lowercase());
            }
            let _ = Origin::parse(&url);
        }
    }

    fn repo(name: &str, description: &str) -> RemoteRepo {
        RemoteRepo {
            full_name: name.into(),
            clone_url: format!("https://example.com/{name}.git"),
            description: description.into(),
            visibility: Visibility::Public,
            default_branch: None,
            updated_at: None,
        }
    }

    #[test]
    fn the_repo_filter_needs_every_word() {
        let repos = vec![
            repo("ana/invoke-git", "A fast git client"),
            repo("ana/notes", "Plain notes"),
            repo("corp/Git-Tools", ""),
        ];
        assert_eq!(filter_repos(&repos, ""), vec![0, 1, 2]);
        assert_eq!(filter_repos(&repos, "  "), vec![0, 1, 2]);
        assert_eq!(filter_repos(&repos, "GIT"), vec![0, 2]);
        assert_eq!(filter_repos(&repos, "git fast"), vec![0]);
        assert_eq!(filter_repos(&repos, "ana notes"), vec![1]);
        assert!(filter_repos(&repos, "zzz").is_empty());
    }

    proptest! {
        #[test]
        fn a_longer_query_never_matches_more(a in "[a-c ]{0,6}", b in "[a-c]{1,3}") {
            let repos = vec![repo("abc/cab", "bca"), repo("a/b", "c"), repo("cc/aa", "")];
            let wide = filter_repos(&repos, &a);
            let narrow = filter_repos(&repos, &format!("{a} {b}"));
            prop_assert!(narrow.iter().all(|at| wide.contains(at)));
        }
    }
}
