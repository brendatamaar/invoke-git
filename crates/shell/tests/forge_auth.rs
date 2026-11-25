//! Forge accounts against mock servers (P8): GitHub device flow, GitLab
//! browser (PKCE) flow, personal access tokens, multi-account host
//! matching, token storage and removal, repo list pagination and error
//! handling.
//!
//! No real network and no real keychain: the "forges" are tiny servers on
//! 127.0.0.1 (std only), tokens rest in a `MemoryStore`, and the account
//! list lives in a generated temp folder.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use invoke_core::forge::{
    AccountBook, AuthMethod, ForgeAccount, ForgeKind, OriginProblem, Resolution, SignInCommand,
    SignInEvent, SignInFailure, SignInMethod, SignInPhase, UserPrompt, Visibility, step_sign_in,
};
use shell::forge::codec::{base64url, parse_query, sha256};
use shell::forge::{
    ClientIds, Credential, ForgeError, ForgeService, GitHubUrls, MemoryStore, Secret,
    ServiceConfig, SignInRunner, TokenStore,
};

// ---------------------------------------------------------------------------
// A tiny HTTP server
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    /// Path and query.
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    fn form(&self) -> Vec<(String, String)> {
        parse_query(&self.body)
    }

    fn form_value(&self, key: &str) -> Option<String> {
        self.form()
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn json(body: &str) -> Reply {
        Reply {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.into(),
        }
    }

    fn status(status: u16, body: &str) -> Reply {
        Reply {
            status,
            ..Reply::json(body)
        }
    }

    fn with(mut self, name: &str, value: &str) -> Reply {
        self.headers.push((name.into(), value.into()));
        self
    }
}

type Handler = Box<dyn Fn(&Recorded) -> Reply + Send + Sync>;

struct Mock {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
}

impl Mock {
    /// Starts a server; `make` gets the port so replies can name the server.
    fn start(make: impl FnOnce(u16) -> Handler) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handler: Arc<Handler> = Arc::new(make(addr.port()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log, halt) = (requests.clone(), stop.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                if halt.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let (handler, log) = (handler.clone(), log.clone());
                thread::spawn(move || serve(stream, &handler, &log));
            }
        });
        Mock {
            addr,
            requests,
            stop,
        }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn requests_to(&self, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.path() == path)
            .collect()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(mut stream: TcpStream, handler: &Handler, log: &Mutex<Vec<Recorded>>) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    let head_end = loop {
        let Ok(read) = stream.read(&mut buffer) else {
            return;
        };
        if read == 0 {
            return;
        }
        data.extend_from_slice(&buffer[..read]);
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (
        first.next().unwrap_or("").to_owned(),
        first.next().unwrap_or("").to_owned(),
    );
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    while data.len() < head_end + length {
        let Ok(read) = stream.read(&mut buffer) else {
            return;
        };
        if read == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read]);
    }
    let body = String::from_utf8_lossy(&data[head_end..]).into_owned();
    let request = Recorded {
        method,
        target,
        headers,
        body,
    };
    log.lock().unwrap().push(request.clone());
    let reply = handler(&request);
    let mut text = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (name, value) in &reply.headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str("\r\n");
    text.push_str(&reply.body);
    let _ = stream.write_all(text.as_bytes());
}

/// A raw GET to a local port, as a browser's redirect would do.
fn browser_get(addr: &str, path_and_query: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path_and_query} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    answer
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "invoke-git-forge-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn book(&self) -> PathBuf {
        self.0.join("forge-accounts.txt")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(origin: &str) -> ServiceConfig {
    ServiceConfig {
        github: GitHubUrls {
            api: origin.into(),
            login: origin.into(),
        },
        client_ids: ClientIds {
            github: Some("gh-client".into()),
            gitlab: Some("gl-client".into()),
        },
        gitlab_loopback_port: 0,
        poll_unit: Duration::from_millis(2),
        gitlab_instance: origin.into(),
        browser_timeout: Duration::from_secs(20),
    }
}

fn service(mock: &Mock, store: Arc<dyn TokenStore>, book: Option<PathBuf>) -> ForgeService {
    ForgeService::new(store, config(&mock.origin()), book).unwrap()
}

/// A GitHub-like server: `/user` answers with the login that goes with the
/// bearer token, and repos come in two pages.
fn github_handler(port: u16) -> Handler {
    Box::new(move |request| {
        match (request.method.as_str(), request.path()) {
        ("GET", "/user") => match request.header("authorization") {
            Some("Bearer ghp_ANA" | "Bearer ghp_ANA_SECOND") => Reply::json(r#"{"login":"ana"}"#),
            Some("Bearer ghp_WORK") => Reply::json(r#"{"login":"ana-corp"}"#),
            _ => Reply::status(401, r#"{"message":"Bad credentials"}"#),
        },
        ("GET", "/user/repos") if request.target.contains("page=2") => Reply::json(
            r#"[{"full_name":"corp/tool","clone_url":"https://github.com/corp/tool.git",
                 "description":"Shared tool","private":true,"visibility":"private",
                 "default_branch":"main","pushed_at":"2026-03-01T00:00:00Z"}]"#,
        ),
        ("GET", "/user/repos") => Reply::json(
            r#"[{"full_name":"ana/invoke-git","clone_url":"https://github.com/ana/invoke-git.git",
                 "description":"A git client","private":false,"visibility":"public",
                 "default_branch":"main","pushed_at":"2026-04-01T00:00:00Z"},
                {"full_name":"ana/evil","clone_url":"ext::sh -c touch% /tmp/pwned"}]"#,
        )
        .with(
            "Link",
            &format!(
                r#"<http://127.0.0.1:{port}/user/repos?page=2>; rel="next", <http://127.0.0.1:{port}/user/repos?page=2>; rel="last""#
            ),
        ),
        _ => Reply::status(404, "{}"),
    }
    })
}

fn memory() -> Arc<MemoryStore> {
    Arc::new(MemoryStore::new())
}

fn loaded(store: &MemoryStore, account: &ForgeAccount) -> Option<Credential> {
    store.load(&account.key()).unwrap()
}

// ---------------------------------------------------------------------------
// Driving the sign-in machine with the runner, as the UI does
// ---------------------------------------------------------------------------

/// Runs one sign-in to its end. `on_prompt` is called once when the machine
/// waits for the user (this is where a test plays the browser).
fn sign_in(
    runner: &SignInRunner,
    method: SignInMethod,
    on_prompt: impl FnMut(&UserPrompt),
) -> SignInPhase {
    sign_in_until(runner, method, on_prompt, || false)
}

/// Like `sign_in`, but also returns (with whatever phase the machine is in)
/// once `stop` says so: for tests that cancel from the outside, where the
/// machine is not told.
fn sign_in_until(
    runner: &SignInRunner,
    method: SignInMethod,
    mut on_prompt: impl FnMut(&UserPrompt),
    stop: impl Fn() -> bool,
) -> SignInPhase {
    let (events, inbox) = mpsc::channel();
    let (mut phase, commands) = step_sign_in(SignInPhase::Idle, SignInEvent::Begin(method));
    for command in commands {
        runner.execute(command, events.clone());
    }
    let mut prompted = false;
    let started = Instant::now();
    while phase.is_active() {
        let event = match inbox.recv_timeout(Duration::from_millis(25)) {
            Ok(event) => event,
            Err(_) if stop() => break,
            Err(_) => {
                assert!(
                    started.elapsed() < Duration::from_secs(15),
                    "the sign-in stalled"
                );
                continue;
            }
        };
        let (next, commands) = step_sign_in(phase, event);
        phase = next;
        if let SignInPhase::AwaitingUser { prompt, .. } = &phase
            && !prompted
        {
            prompted = true;
            on_prompt(prompt);
        }
        for command in commands {
            runner.execute(command, events.clone());
        }
    }
    phase
}

fn device_mock(outcome: &'static str) -> Mock {
    let polls = Arc::new(AtomicUsize::new(0));
    Mock::start(move |port| {
        Box::new(move |request| match request.path() {
            "/login/device/code" => Reply::json(&format!(
                r#"{{"device_code":"dev-secret-code","user_code":"WDJB-MJHT",
                    "verification_uri":"http://127.0.0.1:{port}/login/device",
                    "expires_in":900,"interval":5}}"#
            )),
            "/login/oauth/access_token" => {
                let n = polls.fetch_add(1, Ordering::SeqCst);
                match (n, outcome) {
                    (0, _) => Reply::json(r#"{"error":"authorization_pending"}"#),
                    (1, _) => Reply::json(r#"{"error":"slow_down","interval":10}"#),
                    (_, "denied") => Reply::json(r#"{"error":"access_denied"}"#),
                    (_, "expired") => Reply::json(r#"{"error":"expired_token"}"#),
                    _ => Reply::json(
                        r#"{"access_token":"ghp_ANA","token_type":"bearer","scope":"repo"}"#,
                    ),
                }
            }
            "/user"
                if matches!(
                    request.header("authorization"),
                    Some("Bearer ghp_ANA" | "Bearer ghp_ANA_SECOND")
                ) =>
            {
                Reply::json(r#"{"login":"Ana"}"#)
            }
            _ => Reply::status(404, "{}"),
        })
    })
}

// ---------------------------------------------------------------------------
// GitHub: device flow
// ---------------------------------------------------------------------------

#[test]
fn the_github_device_flow_signs_in_and_keeps_the_token_in_the_store_only() {
    let mock = device_mock("granted");
    let data = TempDir::new("device");
    let store = memory();
    let service = service(&mock, store.clone(), Some(data.book()));
    let runner = SignInRunner::new(service.clone());

    let mut shown = None;
    let phase = sign_in(&runner, SignInMethod::GitHubDevice, |prompt| {
        shown = Some(prompt.clone())
    });

    let SignInPhase::Authorized(account) = phase else {
        panic!("not signed in: {phase:?}");
    };
    assert_eq!(account.kind, ForgeKind::GitHub);
    assert_eq!(account.username, "Ana");
    assert_eq!(account.method, AuthMethod::OAuth);
    assert_eq!(
        shown,
        Some(UserPrompt::DeviceCode {
            user_code: "WDJB-MJHT".into(),
            verification_url: format!("{}/login/device", mock.origin()),
        })
    );
    // The token is in the store, under the account's key.
    let credential = loaded(&store, &account).expect("token stored");
    assert_eq!(credential.access.expose(), "ghp_ANA");
    // The device code request named the app and the scope, and no secret.
    let code_request = &mock.requests_to("/login/device/code")[0];
    assert_eq!(
        code_request.form_value("client_id").as_deref(),
        Some("gh-client")
    );
    assert_eq!(code_request.form_value("scope").as_deref(), Some("repo"));
    assert!(code_request.form_value("client_secret").is_none());
    // Three polls: pending, slow_down, granted; each carries the device code
    // and the grant type, and none a client secret.
    let polls = mock.requests_to("/login/oauth/access_token");
    assert_eq!(polls.len(), 3);
    for poll in &polls {
        assert_eq!(
            poll.form_value("device_code").as_deref(),
            Some("dev-secret-code")
        );
        assert_eq!(
            poll.form_value("grant_type").as_deref(),
            Some("urn:ietf:params:oauth:grant-type:device_code")
        );
        assert!(poll.form_value("client_secret").is_none());
    }
    // Who-am-I used the new token in a header.
    let who = &mock.requests_to("/user")[0];
    assert_eq!(who.header("authorization"), Some("Bearer ghp_ANA"));
    assert!(!who.target.contains("ghp_"));
    // The account list on disk names the account and holds no token.
    let text = std::fs::read_to_string(data.book()).unwrap();
    assert!(text.contains("Ana"));
    assert_no_secret_on_disk(&data.0, &["ghp_ANA", "dev-secret-code"]);
    assert_eq!(service.accounts(), vec![account]);
}

#[test]
fn a_refused_or_expired_device_code_ends_the_flow_and_stores_nothing() {
    for (outcome, failure) in [
        ("denied", SignInFailure::Denied),
        ("expired", SignInFailure::Expired),
    ] {
        let mock = device_mock(outcome);
        let store = memory();
        let runner = SignInRunner::new(service(&mock, store.clone(), None));
        let phase = sign_in(&runner, SignInMethod::GitHubDevice, |_| {});
        assert_eq!(phase, SignInPhase::Failed(failure), "{outcome}");
        assert!(store.is_empty(), "{outcome}");
    }
}

#[test]
fn without_an_oauth_app_id_github_sign_in_says_so_and_sends_nothing() {
    let mock = device_mock("granted");
    let mut config = config(&mock.origin());
    config.client_ids.github = None;
    let service = ForgeService::new(memory(), config, None).unwrap();
    let phase = sign_in(
        &SignInRunner::new(service),
        SignInMethod::GitHubDevice,
        |_| {},
    );
    match phase {
        SignInPhase::Failed(SignInFailure::Error(message)) => {
            assert!(message.contains("access token"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    assert!(mock.requests().is_empty());
}

#[test]
fn cancelling_stops_the_polling() {
    let mock = Mock::start(|port| {
        Box::new(move |request| match request.path() {
            "/login/device/code" => Reply::json(&format!(
                r#"{{"device_code":"d","user_code":"AAAA-BBBB",
                    "verification_uri":"http://127.0.0.1:{port}/device","expires_in":900,"interval":5}}"#
            )),
            _ => Reply::json(r#"{"error":"authorization_pending"}"#),
        })
    });
    // The first poll is due after 5 x 60 ms: a flow that ignored the abort
    // would poll well inside the 700 ms watched below.
    let mut config = config(&mock.origin());
    config.poll_unit = Duration::from_millis(60);
    let service = ForgeService::new(memory(), config, None).unwrap();
    let runner = SignInRunner::new(service);
    let (events, inbox) = mpsc::channel();
    let (phase, commands) = step_sign_in(
        SignInPhase::Idle,
        SignInEvent::Begin(SignInMethod::GitHubDevice),
    );
    for command in commands {
        runner.execute(command, events.clone());
    }
    let ready = inbox.recv_timeout(Duration::from_secs(10)).unwrap();
    let (phase, commands) = step_sign_in(phase, ready);
    assert!(matches!(commands[..], [SignInCommand::PollAfter { .. }]));
    for command in commands {
        runner.execute(command, events.clone());
    }
    let (phase, commands) = step_sign_in(phase, SignInEvent::Cancel);
    assert_eq!(phase, SignInPhase::Failed(SignInFailure::Cancelled));
    assert_eq!(commands, vec![SignInCommand::Abort]);
    for command in commands {
        runner.execute(command, events.clone());
    }
    thread::sleep(Duration::from_millis(700));
    assert!(mock.requests_to("/login/oauth/access_token").is_empty());
    assert!(
        inbox.try_recv().is_err(),
        "a cancelled flow must stay silent"
    );
}

// ---------------------------------------------------------------------------
// GitLab: browser flow with PKCE
// ---------------------------------------------------------------------------

fn gitlab_mock() -> Mock {
    Mock::start(|_| {
        Box::new(|request| match (request.method.as_str(), request.path()) {
            ("POST", "/oauth/token") => match request.form_value("grant_type").as_deref() {
                Some("authorization_code") => Reply::json(
                    r#"{"access_token":"glpat-OAUTH1","refresh_token":"glrt-REFRESH1",
                        "expires_in":7200,"token_type":"Bearer"}"#,
                ),
                _ => Reply::status(400, r#"{"error":"unsupported_grant_type"}"#),
            },
            ("GET", "/api/v4/user") => match request.header("authorization") {
                Some("Bearer glpat-OAUTH1" | "Bearer glpat-PAT") => {
                    Reply::json(r#"{"username":"ana.gl"}"#)
                }
                _ => Reply::status(401, r#"{"message":"401 Unauthorized"}"#),
            },
            _ => Reply::status(404, "{}"),
        })
    })
}

#[test]
fn the_gitlab_browser_flow_uses_pkce_and_checks_the_state() {
    let mock = gitlab_mock();
    let data = TempDir::new("pkce");
    let store = memory();
    let service = service(&mock, store.clone(), Some(data.book()));
    let runner = SignInRunner::new(service.clone());

    let mut authorize = None;
    let phase = sign_in(&runner, SignInMethod::GitLabBrowser, |prompt| {
        let UserPrompt::Browser { url } = prompt else {
            panic!("{prompt:?}");
        };
        let (base, query) = url.split_once('?').unwrap();
        assert_eq!(base, format!("{}/oauth/authorize", mock.origin()));
        let query = parse_query(query);
        let get = |k: &str| {
            query
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{k} missing in {url}"))
        };
        assert_eq!(get("client_id"), "gl-client");
        assert_eq!(get("response_type"), "code");
        assert_eq!(get("code_challenge_method"), "S256");
        assert_eq!(get("scope"), "read_api");
        let redirect = get("redirect_uri");
        let local = redirect
            .strip_prefix("http://")
            .and_then(|r| r.strip_suffix("/callback"))
            .unwrap_or_else(|| panic!("{redirect}"));
        assert!(local.starts_with("127.0.0.1:"));
        let state = get("state");
        authorize = Some(get("code_challenge"));

        // A page that is not the redirect, a redirect with someone else's
        // state and a malformed one are all turned away; the flow goes on.
        assert!(browser_get(local, "/favicon.ico").starts_with("HTTP/1.1 404"));
        assert!(
            browser_get(local, "/callback?code=stolen&state=not-ours").starts_with("HTTP/1.1 400")
        );
        assert!(
            browser_get(local, &format!("/callback?state={state}")).starts_with("HTTP/1.1 400")
        );
        // The real redirect.
        let answer = browser_get(local, &format!("/callback?code=the-code&state={state}"));
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(
            !answer.contains("the-code"),
            "the page must not echo the code"
        );
    });

    let SignInPhase::Authorized(account) = phase else {
        panic!("not signed in: {phase:?}");
    };
    assert_eq!(account.kind, ForgeKind::GitLab);
    assert_eq!(account.username, "ana.gl");
    assert_eq!(account.method, AuthMethod::OAuth);
    assert_eq!(account.origin.as_str(), mock.origin());

    // The exchange carried the code, the redirect and a verifier that hashes
    // to the challenge the browser saw; no client secret.
    let exchange = &mock.requests_to("/oauth/token")[0];
    assert_eq!(exchange.form_value("code").as_deref(), Some("the-code"));
    assert_eq!(
        exchange.form_value("client_id").as_deref(),
        Some("gl-client")
    );
    assert!(exchange.form_value("client_secret").is_none());
    let verifier = exchange.form_value("code_verifier").unwrap();
    assert!(verifier.len() >= 43);
    assert_eq!(base64url(&sha256(verifier.as_bytes())), authorize.unwrap());

    let credential = loaded(&store, &account).unwrap();
    assert_eq!(credential.access.expose(), "glpat-OAUTH1");
    assert_eq!(
        credential.refresh.as_ref().map(Secret::expose),
        Some("glrt-REFRESH1")
    );
    assert!(credential.expires_at.is_some());
    assert_no_secret_on_disk(&data.0, &["glpat-OAUTH1", "glrt-REFRESH1", &verifier]);
}

#[test]
fn declining_in_the_browser_ends_the_flow() {
    let mock = gitlab_mock();
    let store = memory();
    let runner = SignInRunner::new(service(&mock, store.clone(), None));
    let phase = sign_in(&runner, SignInMethod::GitLabBrowser, |prompt| {
        let UserPrompt::Browser { url } = prompt else {
            panic!()
        };
        let query = parse_query(url.split_once('?').unwrap().1);
        let get = |k: &str| query.iter().find(|(key, _)| key == k).unwrap().1.clone();
        let local = get("redirect_uri")
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/callback")
            .unwrap()
            .to_owned();
        browser_get(
            &local,
            &format!("/callback?error=access_denied&state={}", get("state")),
        );
    });
    assert_eq!(phase, SignInPhase::Failed(SignInFailure::Denied));
    assert!(store.is_empty());
    assert!(mock.requests_to("/oauth/token").is_empty());
}

#[test]
fn a_rejected_code_exchange_reports_the_oauth_error_code_only() {
    let mock = Mock::start(|_| {
        Box::new(|request| match request.path() {
            "/oauth/token" => Reply::status(
                400,
                r#"{"error":"invalid_grant","error_description":"leaked-detail the code the-code"}"#,
            ),
            _ => Reply::status(404, "{}"),
        })
    });
    let runner = SignInRunner::new(service(&mock, memory(), None));
    let phase = sign_in(&runner, SignInMethod::GitLabBrowser, |prompt| {
        let UserPrompt::Browser { url } = prompt else {
            panic!()
        };
        let query = parse_query(url.split_once('?').unwrap().1);
        let get = |k: &str| query.iter().find(|(key, _)| key == k).unwrap().1.clone();
        let redirect = get("redirect_uri");
        let local = redirect
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/callback")
            .unwrap();
        browser_get(
            local,
            &format!("/callback?code=the-code&state={}", get("state")),
        );
    });
    match phase {
        SignInPhase::Failed(SignInFailure::Error(message)) => {
            assert!(message.contains("invalid_grant"), "{message}");
            assert!(!message.contains("leaked-detail"));
            assert!(!message.contains("the-code"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_busy_loopback_port_is_reported_not_guessed_around() {
    let mock = gitlab_mock();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = config(&mock.origin());
    config.gitlab_loopback_port = taken.local_addr().unwrap().port();
    let service = ForgeService::new(memory(), config, None).unwrap();
    let phase = sign_in(
        &SignInRunner::new(service),
        SignInMethod::GitLabBrowser,
        |_| {},
    );
    match phase {
        SignInPhase::Failed(SignInFailure::Error(message)) => {
            assert!(message.contains("port"), "{message}")
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Personal access tokens, and the account list
// ---------------------------------------------------------------------------

#[test]
fn a_token_is_checked_before_it_is_kept() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("pat");
    let store = memory();
    let service = service(&mock, store.clone(), Some(data.book()));

    let bad = service.connect_with_token(ForgeKind::GitHub, "", "ghp_WRONG");
    assert_eq!(bad, Err(ForgeError::Unauthorized));
    assert!(store.is_empty());
    assert!(service.accounts().is_empty());
    assert!(
        !data.book().exists(),
        "nothing to write for a failed sign-in"
    );

    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "  ghp_ANA\n")
        .unwrap();
    assert_eq!(
        (account.username.as_str(), account.method),
        ("ana", AuthMethod::Pat)
    );
    assert_eq!(loaded(&store, &account).unwrap().access.expose(), "ghp_ANA");
    assert_no_secret_on_disk(&data.0, &["ghp_ANA"]);

    // Signing in again with the same account replaces, not duplicates.
    service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    assert_eq!(service.accounts().len(), 1);
    assert_eq!(store.len(), 1);
}

#[test]
fn tokens_with_whitespace_and_unsafe_server_addresses_are_refused_up_front() {
    let mock = Mock::start(github_handler);
    let service = service(&mock, memory(), None);
    for token in ["", "  ", "two words", "a\nb"] {
        assert_eq!(
            service.connect_with_token(ForgeKind::GitHub, "", token),
            Err(ForgeError::InvalidToken),
            "{token:?}"
        );
    }
    for (instance, problem) in [
        ("http://gitlab.example.com", OriginProblem::Insecure),
        (
            "https://user:pw@gitlab.example.com",
            OriginProblem::UserInfo,
        ),
        ("https://gitlab.example.com/sub", OriginProblem::Path),
        ("ftp://gitlab.example.com", OriginProblem::Scheme),
    ] {
        assert_eq!(
            service.connect_with_token(ForgeKind::GitLab, instance, "glpat-x"),
            Err(ForgeError::BadOrigin(problem.clone())),
            "{instance}"
        );
    }
    // Nothing reached the network for any of them.
    assert!(mock.requests().is_empty());
}

#[test]
fn a_self_hosted_gitlab_works_with_a_token_and_an_instance_address() {
    let mock = gitlab_mock();
    let store = memory();
    let service = service(&mock, store.clone(), None);
    let account = service
        .connect_with_token(ForgeKind::GitLab, &mock.origin(), "glpat-PAT")
        .unwrap();
    assert_eq!(account.kind, ForgeKind::GitLab);
    assert_eq!(account.origin.as_str(), mock.origin());
    assert_eq!(account.username, "ana.gl");
    // The token went in a header to that instance only.
    let who = &mock.requests_to("/api/v4/user")[0];
    assert_eq!(who.header("authorization"), Some("Bearer glpat-PAT"));
    assert!(!who.target.contains("glpat"));
}

#[test]
fn several_accounts_on_one_host_ask_until_one_is_chosen_and_the_choice_persists() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("multi");
    let store = memory();
    let service = service(&mock, store.clone(), Some(data.book()));
    let personal = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    assert!(matches!(
        service.resolve("https://github.com/o/r.git"),
        Resolution::Use(a) if a == personal
    ));
    let work = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_WORK")
        .unwrap();
    assert_ne!(personal.key(), work.key());
    assert_eq!(store.len(), 2);

    for url in [
        "https://github.com/o/r.git",
        "git@github.com:o/r.git",
        "ssh://git@ssh.github.com:443/o/r.git",
    ] {
        assert!(
            matches!(service.resolve(url), Resolution::Choose(list) if list.len() == 2),
            "{url}"
        );
    }
    assert!(matches!(
        service.resolve("https://gitlab.com/o/r.git"),
        Resolution::NoAccount { .. }
    ));
    assert_eq!(service.resolve(r"C:\work\repo"), Resolution::LocalPath);

    service.prefer(&work.key()).unwrap();
    assert!(matches!(
        service.resolve("https://github.com/o/r.git"),
        Resolution::Use(a) if a == work
    ));
    assert_eq!(
        service.prefer("github|https://github.com|ghost"),
        Err(ForgeError::UnknownAccount)
    );

    // A new service over the same files sees the same accounts and choice.
    let again =
        ForgeService::new(store.clone(), config(&mock.origin()), Some(data.book())).unwrap();
    assert_eq!(again.accounts().len(), 2);
    assert!(matches!(
        again.resolve("https://github.com/o/r.git"),
        Resolution::Use(a) if a == work
    ));
    assert_no_secret_on_disk(&data.0, &["ghp_ANA", "ghp_WORK"]);
}

#[test]
fn disconnecting_removes_the_token_and_the_account() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("disconnect");
    let store = memory();
    let service = service(&mock, store.clone(), Some(data.book()));
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    service.disconnect(&account.key()).unwrap();
    assert!(store.is_empty());
    assert!(service.accounts().is_empty());
    let reread = AccountBook::decode(&std::fs::read_to_string(data.book()).unwrap());
    assert!(reread.is_empty());
    assert_eq!(
        service.disconnect(&account.key()),
        Err(ForgeError::UnknownAccount)
    );
    assert_eq!(
        service.list_repos_page(&account.key(), None),
        Err(ForgeError::UnknownAccount)
    );
}

/// A keychain that refuses to delete.
struct StubbornStore(MemoryStore);

impl TokenStore for StubbornStore {
    fn save(&self, key: &str, credential: &Credential) -> Result<(), ForgeError> {
        self.0.save(key, credential)
    }
    fn load(&self, key: &str) -> Result<Option<Credential>, ForgeError> {
        self.0.load(key)
    }
    fn delete(&self, _: &str) -> Result<(), ForgeError> {
        Err(ForgeError::Keychain(
            "access to the keychain was refused".into(),
        ))
    }
}

/// A keychain that refuses to save.
struct ClosedStore;

impl TokenStore for ClosedStore {
    fn save(&self, _: &str, _: &Credential) -> Result<(), ForgeError> {
        Err(ForgeError::Keychain(
            "no keychain is available on this system".into(),
        ))
    }
    fn load(&self, _: &str) -> Result<Option<Credential>, ForgeError> {
        Ok(None)
    }
    fn delete(&self, _: &str) -> Result<(), ForgeError> {
        Ok(())
    }
}

#[test]
fn a_keychain_that_refuses_never_leaves_a_half_connected_account() {
    let mock = Mock::start(github_handler);
    // Cannot save: no account, no list file.
    let data = TempDir::new("closed");
    let closed = service(&mock, Arc::new(ClosedStore), Some(data.book()));
    let error = closed
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap_err();
    assert!(matches!(error, ForgeError::Keychain(_)));
    assert!(closed.accounts().is_empty());
    assert!(!data.book().exists());
    assert!(!error.user_message().contains("ghp_ANA"));

    // Cannot delete: the account stays, so it is not forgotten with its
    // token still in the keychain.
    let stubborn = service(&mock, Arc::new(StubbornStore(MemoryStore::new())), None);
    let account = stubborn
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    assert!(matches!(
        stubborn.disconnect(&account.key()),
        Err(ForgeError::Keychain(_))
    ));
    assert_eq!(stubborn.accounts(), vec![account]);
}

#[test]
fn an_unwritable_account_list_rolls_the_token_back() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("unwritable");
    // The temporary file the list is written through is a folder, so
    // writing fails while reading (no list yet) works.
    std::fs::create_dir_all(data.book().with_extension("tmp")).unwrap();
    let store = memory();
    let service = service(&mock, store.clone(), Some(data.book()));
    let error = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap_err();
    assert!(matches!(error, ForgeError::Disk(_)), "{error:?}");
    assert!(
        store.is_empty(),
        "the token must not outlive the failed sign-in"
    );
    assert!(service.accounts().is_empty());
}

#[test]
fn a_damaged_account_list_is_survived_and_an_unreadable_one_is_reported() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("damaged");
    std::fs::write(data.book(), "\u{0}\u{0}not an account list\n").unwrap();
    let service = service(&mock, memory(), Some(data.book()));
    assert!(service.accounts().is_empty());
    // A path that cannot be read as a file is an error, not an empty list
    // (an empty list would be saved over the real one).
    let folder = TempDir::new("folder");
    let result = ForgeService::new(memory(), config(&mock.origin()), Some(folder.0.clone()));
    assert!(matches!(result, Err(ForgeError::Disk(_))));
}

#[test]
fn with_no_account_every_remote_stays_anonymous() {
    let mock = Mock::start(github_handler);
    let service = service(&mock, memory(), None);
    assert!(service.accounts().is_empty());
    for url in ["https://github.com/o/r.git", "git@gitlab.com:o/r.git"] {
        assert!(
            matches!(service.resolve(url), Resolution::NoAccount { .. }),
            "{url}"
        );
    }
    assert!(mock.requests().is_empty(), "nothing is contacted unasked");
}

// ---------------------------------------------------------------------------
// Repo lists
// ---------------------------------------------------------------------------

#[test]
fn github_repos_are_listed_across_pages_with_the_token_in_a_header() {
    let mock = Mock::start(github_handler);
    let service = service(&mock, memory(), None);
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();

    let listing = service.list_repos(&account.key(), 10).unwrap();
    assert_eq!(listing.more, None);
    // The `ext::` address on page one was dropped; page two followed.
    let names: Vec<&str> = listing.repos.iter().map(|r| r.full_name.as_str()).collect();
    assert_eq!(names, ["ana/invoke-git", "corp/tool"]);
    assert_eq!(listing.repos[0].visibility, Visibility::Public);
    assert_eq!(listing.repos[1].visibility, Visibility::Private);
    assert_eq!(
        listing.repos[0].clone_url,
        "https://github.com/ana/invoke-git.git"
    );
    assert_eq!(listing.repos[1].default_branch.as_deref(), Some("main"));

    let pages = mock.requests_to("/user/repos");
    assert_eq!(pages.len(), 2);
    assert!(pages[0].target.contains("per_page=100"));
    for page in &pages {
        assert_eq!(page.header("authorization"), Some("Bearer ghp_ANA"));
        assert_eq!(page.header("accept"), Some("application/vnd.github+json"));
        assert!(!page.target.contains("ghp_"));
    }
}

#[test]
fn the_page_limit_stops_the_listing_and_says_where_it_would_go_on() {
    let mock = Mock::start(github_handler);
    let service = service(&mock, memory(), None);
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    let first = service.list_repos(&account.key(), 1).unwrap();
    assert_eq!(first.repos.len(), 1);
    let more = first.more.expect("a next page remains");
    let page = service
        .list_repos_page(&account.key(), Some(&more))
        .unwrap();
    assert_eq!(page.repos.len(), 1);
    assert_eq!(page.next, None);
    // A cursor that points at another server is refused, token unsent.
    assert_eq!(
        service.list_repos_page(&account.key(), Some("https://evil.example/user/repos")),
        Err(ForgeError::InsecureUrl)
    );
}

#[test]
fn a_link_to_another_server_is_never_followed() {
    let thief = Mock::start(|_| Box::new(|_| Reply::json("[]")));
    let thief_origin = thief.origin();
    let mock = Mock::start(move |_| {
        let thief_origin = thief_origin.clone();
        Box::new(move |request| match request.path() {
            "/user" => Reply::json(r#"{"login":"ana"}"#),
            _ => Reply::json("[]").with("Link", &format!(r#"<{thief_origin}/steal>; rel="next""#)),
        })
    });
    let service = service(&mock, memory(), None);
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    let listing = service.list_repos(&account.key(), 5).unwrap();
    assert_eq!(listing.more, None);
    assert!(
        thief.requests().is_empty(),
        "the token must not leave its server"
    );
}

#[test]
fn a_redirect_is_not_followed_with_the_token() {
    let elsewhere = Mock::start(|_| Box::new(|_| Reply::json(r#"{"login":"x"}"#)));
    let target = format!("{}/user", elsewhere.origin());
    let mock = Mock::start(move |_| {
        let target = target.clone();
        Box::new(move |_| Reply::status(302, "").with("Location", &target))
    });
    let service = service(&mock, memory(), None);
    let result = service.connect_with_token(ForgeKind::GitHub, "", "ghp_ANA");
    assert_eq!(result, Err(ForgeError::Status(302)));
    assert!(elsewhere.requests().is_empty());
}

#[test]
fn gitlab_projects_are_listed_with_their_groups() {
    let mock = Mock::start(|port| {
        Box::new(move |request| {
            match (request.path(), request.target.contains("page=2")) {
            ("/api/v4/user", _) => Reply::json(r#"{"username":"ana.gl"}"#),
            ("/api/v4/projects", false) => Reply::json(
                r#"[{"path_with_namespace":"grp/sub/tool","http_url_to_repo":"http://127.0.0.1:1/grp/sub/tool.git",
                     "description":null,"visibility":"internal","default_branch":"trunk",
                     "last_activity_at":"2026-05-05T00:00:00Z"}]"#,
            )
            .with("Link", &format!(r#"<http://127.0.0.1:{port}/api/v4/projects?page=2>; rel="next""#)),
            ("/api/v4/projects", true) => Reply::json(
                r#"[{"path_with_namespace":"ana.gl/solo","http_url_to_repo":"http://127.0.0.1:1/ana.gl/solo.git",
                     "visibility":"public"}]"#,
            ),
            _ => Reply::status(404, "{}"),
        }
        })
    });
    let service = service(&mock, memory(), None);
    let account = service
        .connect_with_token(ForgeKind::GitLab, &mock.origin(), "glpat-x")
        .unwrap();
    let listing = service.list_repos(&account.key(), 5).unwrap();
    let names: Vec<&str> = listing.repos.iter().map(|r| r.full_name.as_str()).collect();
    assert_eq!(names, ["grp/sub/tool", "ana.gl/solo"]);
    assert_eq!(listing.repos[0].visibility, Visibility::Internal);
    assert_eq!(listing.repos[1].visibility, Visibility::Public);
    let first = &mock.requests_to("/api/v4/projects")[0];
    assert!(first.target.contains("membership=true"));
    assert!(first.target.contains("per_page=100"));
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn failing_listing(
    reply: impl Fn() -> Reply + Send + Sync + 'static,
) -> (Mock, ForgeService, String) {
    let mock = Mock::start(move |_| {
        Box::new(move |request| match request.path() {
            "/user" => Reply::json(r#"{"login":"ana"}"#),
            _ => reply(),
        })
    });
    let service = service(&mock, memory(), None);
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_SECRETTOKEN")
        .unwrap();
    let key = account.key();
    (mock, service, key)
}

#[test]
fn api_failures_become_errors_the_app_can_act_on_and_none_shows_the_token() {
    type Case = (fn() -> Reply, ForgeError);
    let cases: Vec<Case> = vec![
        (
            || Reply::status(401, r#"{"message":"bad ghp_SECRETTOKEN"}"#),
            ForgeError::Unauthorized,
        ),
        (|| Reply::status(403, "{}"), ForgeError::Forbidden),
        (
            || Reply::status(403, "{}").with("x-ratelimit-remaining", "0"),
            ForgeError::RateLimited {
                retry_after_secs: None,
            },
        ),
        (
            || Reply::status(429, "{}").with("retry-after", "42"),
            ForgeError::RateLimited {
                retry_after_secs: Some(42),
            },
        ),
        (|| Reply::status(404, "{}"), ForgeError::NotFound),
        (
            || Reply::status(503, "oops ghp_SECRETTOKEN"),
            ForgeError::Server(503),
        ),
        (|| Reply::status(418, "{}"), ForgeError::Status(418)),
        (
            || Reply::json("this is not json ghp_SECRETTOKEN"),
            ForgeError::BadResponse("not JSON"),
        ),
        (
            || Reply::json(r#"{"not":"a list"}"#),
            ForgeError::BadResponse("not a list"),
        ),
    ];
    for (reply, expected) in cases {
        let (_mock, service, key) = failing_listing(reply);
        let error = service.list_repos_page(&key, None).unwrap_err();
        assert_eq!(error, expected);
        for text in [error.to_string(), format!("{error:?}")] {
            assert!(!text.contains("SECRETTOKEN"), "{text}");
        }
    }
    assert!(ForgeError::Unauthorized.needs_reconnect());
}

#[test]
fn an_unreachable_server_is_a_network_error_without_the_address() {
    // Bind and drop to find a port nothing listens on.
    let dead = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let mock = Mock::start(github_handler);
    let service = ForgeService::new(
        memory(),
        ServiceConfig {
            github: GitHubUrls {
                api: format!("http://{dead}"),
                login: format!("http://{dead}"),
            },
            ..config(&mock.origin())
        },
        None,
    )
    .unwrap();
    let error = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap_err();
    assert_eq!(error, ForgeError::Network);
    assert!(!error.user_message().contains(&dead.port().to_string()));
}

#[test]
fn a_token_is_never_sent_over_plain_http_to_a_real_host() {
    let mock = Mock::start(github_handler);
    let service = ForgeService::new(
        memory(),
        ServiceConfig {
            github: GitHubUrls {
                api: "http://api.github.com".into(),
                login: "http://github.com".into(),
            },
            ..config(&mock.origin())
        },
        None,
    )
    .unwrap();
    assert_eq!(
        service.connect_with_token(ForgeKind::GitHub, "", "ghp_ANA"),
        Err(ForgeError::InsecureUrl)
    );
}

// ---------------------------------------------------------------------------
// GitLab token refresh
// ---------------------------------------------------------------------------

struct Refreshing {
    mock: Mock,
    service: ForgeService,
    store: Arc<MemoryStore>,
    account: ForgeAccount,
    now: Arc<AtomicUsize>,
}

/// A GitLab OAuth account whose access token runs out at t=1000. The server
/// accepts only the refreshed token on the API.
fn refreshing(refresh_answer: &'static str) -> Refreshing {
    let mock = Mock::start(move |_| {
        Box::new(
            move |request| match (request.method.as_str(), request.path()) {
                ("POST", "/oauth/token") => {
                    if request.form_value("grant_type").as_deref() == Some("refresh_token")
                        && request.form_value("refresh_token").as_deref() == Some("glrt-OLD")
                    {
                        Reply::json(refresh_answer)
                    } else {
                        Reply::status(400, r#"{"error":"invalid_grant"}"#)
                    }
                }
                ("GET", "/api/v4/user") => Reply::json(r#"{"username":"ana.gl"}"#),
                ("GET", "/api/v4/projects") => {
                    if request.header("authorization") == Some("Bearer glpat-NEW") {
                        Reply::json(r#"[]"#)
                    } else {
                        Reply::status(401, "{}")
                    }
                }
                _ => Reply::status(404, "{}"),
            },
        )
    });
    let store = memory();
    let now = Arc::new(AtomicUsize::new(500));
    let clock = now.clone();
    let service =
        ForgeService::with_clock(store.clone(), config(&mock.origin()), None, move || {
            clock.load(Ordering::SeqCst) as u64
        })
        .unwrap();
    let account = service
        .connect_with_token(ForgeKind::GitLab, &mock.origin(), "glpat-OLD")
        .unwrap();
    store
        .save(
            &account.key(),
            &Credential {
                access: Secret::new("glpat-OLD").unwrap(),
                refresh: Secret::new("glrt-OLD"),
                expires_at: Some(1000),
            },
        )
        .unwrap();
    Refreshing {
        mock,
        service,
        store,
        account,
        now,
    }
}

#[test]
fn an_expired_gitlab_token_is_refreshed_before_the_call() {
    let r =
        refreshing(r#"{"access_token":"glpat-NEW","refresh_token":"glrt-NEW","expires_in":7200}"#);
    // Before expiry the old token is used as it is (and the server rejects
    // it, which triggers the retry path tested below); move past expiry.
    r.now.store(2000, Ordering::SeqCst);
    r.service.list_repos_page(&r.account.key(), None).unwrap();
    let refresh = &r.mock.requests_to("/oauth/token")[0];
    assert_eq!(
        refresh.form_value("client_id").as_deref(),
        Some("gl-client")
    );
    assert!(refresh.form_value("client_secret").is_none());
    let call = &r.mock.requests_to("/api/v4/projects")[0];
    assert_eq!(call.header("authorization"), Some("Bearer glpat-NEW"));
    let saved = loaded(&r.store, &r.account).unwrap();
    assert_eq!(saved.access.expose(), "glpat-NEW");
    assert_eq!(saved.refresh.as_ref().map(Secret::expose), Some("glrt-NEW"));
    assert_eq!(saved.expires_at, Some(2000 + 7200));
}

#[test]
fn a_refresh_that_leaves_out_the_new_refresh_token_keeps_the_old_one() {
    let r = refreshing(r#"{"access_token":"glpat-NEW","expires_in":7200}"#);
    r.now.store(2000, Ordering::SeqCst);
    r.service.list_repos_page(&r.account.key(), None).unwrap();
    let saved = loaded(&r.store, &r.account).unwrap();
    assert_eq!(saved.refresh.as_ref().map(Secret::expose), Some("glrt-OLD"));
}

#[test]
fn a_rejected_token_is_refreshed_once_and_the_call_retried() {
    let r =
        refreshing(r#"{"access_token":"glpat-NEW","refresh_token":"glrt-NEW","expires_in":7200}"#);
    // The clock says the old token is fine; the server says 401.
    r.service.list_repos_page(&r.account.key(), None).unwrap();
    let calls = r.mock.requests_to("/api/v4/projects");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].header("authorization"), Some("Bearer glpat-OLD"));
    assert_eq!(calls[1].header("authorization"), Some("Bearer glpat-NEW"));
    assert_eq!(r.mock.requests_to("/oauth/token").len(), 1);
}

#[test]
fn a_revoked_grant_asks_the_user_to_reconnect() {
    // The server only honors "glrt-OLD"; the stored one differs.
    let r = refreshing(r#"{"access_token":"glpat-NEW"}"#);
    r.store
        .save(
            &r.account.key(),
            &Credential {
                access: Secret::new("glpat-OLD").unwrap(),
                refresh: Secret::new("glrt-REVOKED"),
                expires_at: Some(1000),
            },
        )
        .unwrap();
    r.now.store(2000, Ordering::SeqCst);
    let error = r
        .service
        .list_repos_page(&r.account.key(), None)
        .unwrap_err();
    assert_eq!(error, ForgeError::Unauthorized);
    assert!(error.needs_reconnect());
}

#[test]
fn a_missing_keychain_entry_is_reported_as_such() {
    let r = refreshing(r#"{"access_token":"glpat-NEW"}"#);
    r.store.delete(&r.account.key()).unwrap();
    assert_eq!(
        r.service.list_repos_page(&r.account.key(), None),
        Err(ForgeError::NoCredential)
    );
}

// ---------------------------------------------------------------------------

/// No file under `dir` may contain any of the secrets: tokens belong in the
/// keychain only.
fn assert_no_secret_on_disk(dir: &Path, secrets: &[&str]) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
            for secret in secrets {
                assert!(!text.contains(secret), "{} holds a secret", path.display());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cross review fixes
// ---------------------------------------------------------------------------

/// A keychain that runs a hook right after each save.
#[derive(Default)]
struct HookStore {
    inner: MemoryStore,
    after_save: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Makes every delete fail (a keychain that refuses).
    fail_delete: AtomicBool,
    /// Makes every load fail (a keychain that went away).
    fail_load: AtomicBool,
}

impl TokenStore for HookStore {
    fn save(&self, key: &str, credential: &Credential) -> Result<(), ForgeError> {
        self.inner.save(key, credential)?;
        if let Some(hook) = self.after_save.lock().unwrap().as_ref() {
            hook();
        }
        Ok(())
    }
    fn load(&self, key: &str) -> Result<Option<Credential>, ForgeError> {
        if self.fail_load.load(Ordering::SeqCst) {
            return Err(ForgeError::Keychain(
                "no keychain is available on this system".into(),
            ));
        }
        self.inner.load(key)
    }
    fn delete(&self, key: &str) -> Result<(), ForgeError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(ForgeError::Keychain(
                "access to the keychain was refused".into(),
            ));
        }
        self.inner.delete(key)
    }
}

/// Waits (up to 5 s) for something that happens on another thread.
fn eventually(what: &str, condition: impl Fn() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(started.elapsed() < Duration::from_secs(5), "{what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn github_with_login(login: String) -> Mock {
    Mock::start(move |_| {
        Box::new(move |request| match request.path() {
            "/user" => Reply::json(&format!(r#"{{"login":{login:?}}}"#)),
            _ => Reply::status(404, "{}"),
        })
    })
}

#[test]
fn a_user_name_the_server_made_up_is_refused() {
    for login in [
        "ghp_ANA".to_string(),
        "x-ghp_ANA-y".to_string(),
        "ana corp".to_string(),
        "ana\tcorp".to_string(),
        "<b>ana</b>".to_string(),
        "ana/../etc".to_string(),
        String::new(),
        "a".repeat(101),
    ] {
        let mock = github_with_login(login.clone());
        let data = TempDir::new("username");
        let store = memory();
        let service = service(&mock, store.clone(), Some(data.book()));
        let result = service.connect_with_token(ForgeKind::GitHub, "", "ghp_ANA");
        assert_eq!(
            result,
            Err(ForgeError::BadResponse("unusable user name")),
            "{login:?}"
        );
        assert!(store.is_empty(), "{login:?}");
        assert!(service.accounts().is_empty(), "{login:?}");
        assert!(!data.book().exists(), "{login:?}");
    }
    for login in ["ana", "ana.gl", "Ana_Corp-2"] {
        let mock = github_with_login(login.to_string());
        let service = service(&mock, memory(), None);
        assert!(
            service
                .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
                .is_ok(),
            "{login}"
        );
    }
}

#[test]
fn oauth_error_text_from_a_server_is_never_shown() {
    let mock = Mock::start(|_| {
        Box::new(|request| match request.path() {
            "/login/device/code" => Reply::status(
                400,
                r#"{"error":"ghp_SECRET_hostile_text_that_looks_like_a_token"}"#,
            ),
            _ => Reply::status(404, "{}"),
        })
    });
    let runner = SignInRunner::new(service(&mock, memory(), None));
    let phase = sign_in(&runner, SignInMethod::GitHubDevice, |_| {});
    match phase {
        SignInPhase::Failed(SignInFailure::Error(message)) => {
            assert!(message.contains("unknown_error"), "{message}");
            assert!(!message.contains("ghp_SECRET"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_cancel_from_another_thread_while_the_token_is_saved_leaves_nothing_connected() {
    let mock = device_mock("granted");
    let data = TempDir::new("cancel-write");
    let store = Arc::new(HookStore::default());
    let service = service(&mock, store.clone(), Some(data.book()));
    let runner = Arc::new(SignInRunner::new(service.clone()));
    let fired = Arc::new(AtomicBool::new(false));
    {
        // The user presses Cancel (another thread) at the moment the keychain
        // write lands. It waits for the commit, then undoes it.
        let (runner, fired) = (runner.clone(), fired.clone());
        let (sender, _unused) = mpsc::channel();
        *store.after_save.lock().unwrap() = Some(Box::new(move || {
            let (runner, sender) = (runner.clone(), sender.clone());
            thread::spawn(move || runner.execute(SignInCommand::Abort, sender));
            fired.store(true, Ordering::SeqCst);
        }));
    }
    sign_in_until(
        &runner,
        SignInMethod::GitHubDevice,
        |_| {},
        || fired.load(Ordering::SeqCst),
    );
    assert!(fired.load(Ordering::SeqCst), "the token was never saved");
    eventually("the cancelled sign-in must be rolled back", || {
        store.inner.is_empty() && service.accounts().is_empty()
    });
    if let Ok(text) = std::fs::read_to_string(data.book()) {
        assert!(AccountBook::decode(&text).is_empty());
    }
}

#[test]
fn cancelling_during_the_code_exchange_stores_nothing() {
    let slot: Arc<Mutex<Option<Arc<SignInRunner>>>> = Arc::default();
    let hook = slot.clone();
    let mock = Mock::start(move |_| {
        Box::new(move |request| match request.path() {
            "/oauth/token" => {
                // Cancel arrives while the server is answering the exchange.
                if let Some(runner) = hook.lock().unwrap().as_ref() {
                    let (sender, _unused) = mpsc::channel();
                    runner.execute(SignInCommand::Abort, sender);
                }
                Reply::json(r#"{"access_token":"glpat-LATE","expires_in":7200}"#)
            }
            "/api/v4/user" => Reply::json(r#"{"username":"ana.gl"}"#),
            _ => Reply::status(404, "{}"),
        })
    });
    let store = memory();
    let service = service(&mock, store.clone(), None);
    let runner = Arc::new(SignInRunner::new(service.clone()));
    *slot.lock().unwrap() = Some(runner.clone());
    sign_in_until(
        &runner,
        SignInMethod::GitLabBrowser,
        |prompt| {
            let UserPrompt::Browser { url } = prompt else {
                panic!()
            };
            let query = parse_query(url.split_once('?').unwrap().1);
            let get = |k: &str| query.iter().find(|(key, _)| key == k).unwrap().1.clone();
            let redirect = get("redirect_uri");
            let local = redirect
                .strip_prefix("http://")
                .unwrap()
                .strip_suffix("/callback")
                .unwrap();
            browser_get(local, &format!("/callback?code=c&state={}", get("state")));
        },
        || !mock.requests_to("/oauth/token").is_empty(),
    );
    thread::sleep(Duration::from_millis(400));
    assert!(store.is_empty());
    assert!(service.accounts().is_empty());
    assert!(
        mock.requests_to("/api/v4/user").is_empty(),
        "a cancelled flow must not go on to look the user up"
    );
}

// -- a refresh must not undo a disconnect -----------------------------------

type Hook = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

fn refresh_mock(hook: Hook) -> Mock {
    Mock::start(move |_| {
        Box::new(move |request| match request.path() {
            "/oauth/token" => {
                if let Some(run) = hook.lock().unwrap().as_ref() {
                    run();
                }
                Reply::json(
                    r#"{"access_token":"glpat-NEW","refresh_token":"glrt-NEW","expires_in":7200}"#,
                )
            }
            "/api/v4/user" => Reply::json(r#"{"username":"ana.gl"}"#),
            "/api/v4/projects" => Reply::json("[]"),
            _ => Reply::status(404, "{}"),
        })
    })
}

/// A GitLab account whose access token ran out (the clock says 2000).
fn expired_account(mock: &Mock, store: &Arc<MemoryStore>) -> (ForgeService, ForgeAccount) {
    let service =
        ForgeService::with_clock(store.clone(), config(&mock.origin()), None, || 2000).unwrap();
    let account = service
        .connect_with_token(ForgeKind::GitLab, &mock.origin(), "glpat-OLD")
        .unwrap();
    store
        .save(
            &account.key(),
            &Credential {
                access: Secret::new("glpat-OLD").unwrap(),
                refresh: Secret::new("glrt-OLD"),
                expires_at: Some(1000),
            },
        )
        .unwrap();
    (service, account)
}

#[test]
fn a_refresh_never_brings_back_an_account_disconnected_meanwhile() {
    let hook: Hook = Arc::default();
    let mock = refresh_mock(hook.clone());
    let store = memory();
    let (service, account) = expired_account(&mock, &store);
    let (again, key) = (service.clone(), account.key());
    *hook.lock().unwrap() = Some(Box::new(move || {
        again.disconnect(&key).unwrap();
    }));
    let error = service.list_repos_page(&account.key(), None).unwrap_err();
    assert_eq!(error, ForgeError::UnknownAccount);
    assert!(
        store.is_empty(),
        "the refreshed token was written back to the keychain"
    );
    assert!(service.accounts().is_empty());
}

#[test]
fn a_refresh_does_not_overwrite_a_newer_sign_in() {
    let hook: Hook = Arc::default();
    let mock = refresh_mock(hook.clone());
    let store = memory();
    let (service, account) = expired_account(&mock, &store);
    let (again, key) = (store.clone(), account.key());
    *hook.lock().unwrap() = Some(Box::new(move || {
        again
            .save(
                &key,
                &Credential::token(Secret::new("glpat-REPLACED").unwrap()),
            )
            .unwrap();
    }));
    service.list_repos_page(&account.key(), None).unwrap();
    let kept = loaded(&store, &account).unwrap();
    assert_eq!(kept.access.expose(), "glpat-REPLACED");
}

// -- registering and disconnecting are undone cleanly --------------------------

/// Makes writing the account list fail (its temporary file is a folder).
fn break_list(data: &TempDir) {
    std::fs::create_dir_all(data.book().with_extension("tmp")).unwrap();
}

#[test]
fn a_failed_reconnect_puts_the_previous_token_back() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("reconnect");
    let store = memory();
    let service = service(&mock, store.clone(), Some(data.book()));
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    break_list(&data);
    let error = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA_SECOND")
        .unwrap_err();
    assert!(matches!(error, ForgeError::Disk(_)), "{error:?}");
    // The account still has the token it had, not the one that failed.
    assert_eq!(loaded(&store, &account).unwrap().access.expose(), "ghp_ANA");
    assert_eq!(service.accounts(), vec![account]);
}

#[test]
fn an_undo_that_fails_is_reported_not_hidden() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("rollback");
    break_list(&data);
    let stubborn = Arc::new(StubbornStore(MemoryStore::new()));
    let service = service(&mock, stubborn.clone(), Some(data.book()));
    let error = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap_err();
    assert_eq!(error, ForgeError::RollbackFailed);
    let message = error.user_message();
    assert!(message.contains("keychain"), "{message}");
    assert!(!message.contains("ghp_ANA"));
    assert!(service.accounts().is_empty());
}

/// Records what the account list on disk said when the token was deleted.
struct ProbeStore {
    inner: MemoryStore,
    book: PathBuf,
    listed_at_delete: Mutex<Option<bool>>,
}

impl TokenStore for ProbeStore {
    fn save(&self, key: &str, credential: &Credential) -> Result<(), ForgeError> {
        self.inner.save(key, credential)
    }
    fn load(&self, key: &str) -> Result<Option<Credential>, ForgeError> {
        self.inner.load(key)
    }
    fn delete(&self, key: &str) -> Result<(), ForgeError> {
        let text = std::fs::read_to_string(&self.book).unwrap_or_default();
        self.listed_at_delete
            .lock()
            .unwrap()
            .get_or_insert(!AccountBook::decode(&text).is_empty());
        self.inner.delete(key)
    }
}

#[test]
fn disconnecting_removes_the_account_from_the_list_before_the_token() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("order");
    let probe = Arc::new(ProbeStore {
        inner: MemoryStore::new(),
        book: data.book(),
        listed_at_delete: Mutex::new(None),
    });
    let service = service(&mock, probe.clone(), Some(data.book()));
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    service.disconnect(&account.key()).unwrap();
    // A restart between the two steps must not find a token-less account.
    assert_eq!(*probe.listed_at_delete.lock().unwrap(), Some(false));
}

#[test]
fn a_disconnect_that_cannot_save_the_list_keeps_the_token() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("disconnect-disk");
    let store = memory();
    let service = service(&mock, store.clone(), Some(data.book()));
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    break_list(&data);
    let error = service.disconnect(&account.key()).unwrap_err();
    assert!(matches!(error, ForgeError::Disk(_)), "{error:?}");
    assert!(loaded(&store, &account).is_some());
    assert_eq!(service.accounts(), vec![account]);
}

#[test]
fn a_refused_token_delete_puts_the_account_back_on_disk_too() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("disconnect-keychain");
    let service = service(
        &mock,
        Arc::new(StubbornStore(MemoryStore::new())),
        Some(data.book()),
    );
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    assert!(matches!(
        service.disconnect(&account.key()),
        Err(ForgeError::Keychain(_))
    ));
    // After a restart the account is still there, with its token.
    let text = std::fs::read_to_string(data.book()).unwrap();
    assert_eq!(AccountBook::decode(&text).accounts().len(), 1);
}

// -- the loopback listener never outlives the flow -----------------------------

#[test]
fn a_stalled_connection_cannot_hold_the_port_past_cancel() {
    let mock = gitlab_mock();
    let runner = SignInRunner::new(service(&mock, memory(), None));
    let released = Arc::new(AtomicBool::new(false));
    let flag = released.clone();
    let stalled = Mutex::new(None);
    sign_in_until(
        &runner,
        SignInMethod::GitLabBrowser,
        |prompt| {
            let UserPrompt::Browser { url } = prompt else {
                panic!()
            };
            let query = parse_query(url.split_once('?').unwrap().1);
            let redirect = query
                .iter()
                .find(|(k, _)| k == "redirect_uri")
                .unwrap()
                .1
                .clone();
            let local = redirect
                .strip_prefix("http://")
                .unwrap()
                .strip_suffix("/callback")
                .unwrap()
                .to_owned();
            // Connect and say nothing; give the listener time to take it.
            *stalled.lock().unwrap() = Some(TcpStream::connect(&local).unwrap());
            thread::sleep(Duration::from_millis(150));
            let (sender, _unused) = mpsc::channel();
            runner.execute(SignInCommand::Abort, sender);
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(1500) {
                if TcpListener::bind(&local).is_ok() {
                    flag.store(true, Ordering::SeqCst);
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        },
        || true,
    );
    assert!(
        released.load(Ordering::SeqCst),
        "the port stayed taken after Cancel"
    );
}

#[test]
fn a_stalled_connection_cannot_hold_the_flow_past_its_deadline() {
    let mock = gitlab_mock();
    let mut config = config(&mock.origin());
    config.browser_timeout = Duration::from_millis(300);
    let service = ForgeService::new(memory(), config, None).unwrap();
    let runner = SignInRunner::new(service);
    let stalled = Mutex::new(None);
    let started = Instant::now();
    let phase = sign_in(&runner, SignInMethod::GitLabBrowser, |prompt| {
        let UserPrompt::Browser { url } = prompt else {
            panic!()
        };
        let query = parse_query(url.split_once('?').unwrap().1);
        let redirect = query
            .iter()
            .find(|(k, _)| k == "redirect_uri")
            .unwrap()
            .1
            .clone();
        let local = redirect
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/callback")
            .unwrap()
            .to_owned();
        *stalled.lock().unwrap() = Some(TcpStream::connect(local).unwrap());
    });
    assert_eq!(phase, SignInPhase::Failed(SignInFailure::Expired));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the deadline waited for a silent connection: {:?}",
        started.elapsed()
    );
}

// -- cancel and commit exclude each other ----------------------------------------

/// Runs a device flow until its account is stored, without reading the
/// `Granted` event that follows: the user has not been told yet, so a Cancel
/// now still reads "cancelled" on screen.
fn run_until_committed(runner: &SignInRunner, service: &ForgeService) {
    let (events, inbox) = mpsc::channel();
    let (mut phase, commands) = step_sign_in(
        SignInPhase::Idle,
        SignInEvent::Begin(SignInMethod::GitHubDevice),
    );
    for command in commands {
        runner.execute(command, events.clone());
    }
    let started = Instant::now();
    while service.accounts().is_empty() {
        assert!(started.elapsed() < Duration::from_secs(15), "never stored");
        if let Ok(event) = inbox.recv_timeout(Duration::from_millis(5)) {
            let (next, commands) = step_sign_in(phase, event);
            phase = next;
            for command in commands {
                runner.execute(command, events.clone());
            }
        }
    }
}

fn abort(runner: &SignInRunner) {
    let (sender, _unused) = mpsc::channel();
    runner.execute(SignInCommand::Abort, sender);
}

#[test]
fn a_cancel_after_the_commit_undoes_that_commit() {
    let mock = device_mock("granted");
    let data = TempDir::new("late-cancel");
    let store = memory();
    let service = service(&mock, store.clone(), Some(data.book()));
    let runner = SignInRunner::new(service.clone());
    run_until_committed(&runner, &service);
    abort(&runner);
    // Cancel returned: what is stored already agrees with "cancelled".
    assert!(service.accounts().is_empty());
    assert!(store.is_empty());
    let text = std::fs::read_to_string(data.book()).unwrap();
    assert!(AccountBook::decode(&text).is_empty());
}

#[test]
fn a_late_cancel_does_not_undo_a_newer_sign_in() {
    let mock = device_mock("granted");
    let store = memory();
    let service = service(&mock, store.clone(), None);
    let runner = SignInRunner::new(service.clone());
    run_until_committed(&runner, &service);
    // Meanwhile the user signs in with a token for the same account.
    let newer = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA_SECOND")
        .unwrap();
    abort(&runner);
    assert_eq!(service.accounts(), vec![newer.clone()]);
    let kept = loaded(&store, &newer).unwrap();
    assert_eq!(kept.access.expose(), "ghp_ANA_SECOND");
    assert_eq!(newer.method, AuthMethod::Pat);
}

#[test]
fn a_late_cancel_does_not_bring_back_a_disconnected_account() {
    let mock = device_mock("granted");
    let store = memory();
    let service = service(&mock, store.clone(), None);
    let runner = SignInRunner::new(service.clone());
    run_until_committed(&runner, &service);
    let key = service.accounts()[0].key();
    service.disconnect(&key).unwrap();
    abort(&runner);
    assert!(service.accounts().is_empty());
    assert!(store.is_empty());
}

#[test]
fn cancel_before_the_commit_stores_nothing_even_for_a_second_sign_in() {
    // A cancelled flow followed at once by a token sign-in: the token
    // sign-in is untouched by the dead flow.
    let mock = device_mock("granted");
    let store = memory();
    let service = service(&mock, store.clone(), None);
    let runner = SignInRunner::new(service.clone());
    let (events, _inbox) = mpsc::channel();
    runner.execute(SignInCommand::Request(SignInMethod::GitHubDevice), events);
    abort(&runner);
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA_SECOND")
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(service.accounts(), vec![account.clone()]);
    assert_eq!(
        loaded(&store, &account).unwrap().access.expose(),
        "ghp_ANA_SECOND"
    );
}

#[test]
fn a_disconnect_cannot_interleave_with_a_registration() {
    let mock = Mock::start(github_handler);
    let store = Arc::new(HookStore::default());
    let service = service(&mock, store.clone(), None);
    let result: Arc<Mutex<Option<Result<(), ForgeError>>>> = Arc::default();
    {
        // Right after the token is written (the account is not listed yet) a
        // disconnect for the same account is attempted from another thread.
        let (service, result) = (service.clone(), result.clone());
        *store.after_save.lock().unwrap() = Some(Box::new(move || {
            let (service, result) = (service.clone(), result.clone());
            thread::spawn(move || {
                let outcome = service.disconnect("github|https://github.com|ana");
                *result.lock().unwrap() = Some(outcome);
            });
            // Give the other thread time to run before the write goes on.
            thread::sleep(Duration::from_millis(150));
        }));
    }
    service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    eventually("the disconnect never finished", || {
        result.lock().unwrap().is_some()
    });
    // It waited for the registration, then removed what it had stored: no
    // account without a token, no token without an account.
    assert_eq!(result.lock().unwrap().clone(), Some(Ok(())));
    assert!(service.accounts().is_empty());
    assert!(store.inner.is_empty());
}

#[test]
fn a_user_name_holding_the_refresh_token_is_refused_whatever_its_case() {
    // The server reports a name made of the refresh token, in other case.
    let mock = Mock::start(|_| {
        Box::new(|request| match request.path() {
            "/oauth/token" => Reply::json(
                r#"{"access_token":"glpat-OAUTH1","refresh_token":"glrt-REFRESH1","expires_in":7200}"#,
            ),
            "/api/v4/user" => Reply::json(r#"{"username":"x.GLRT-refresh1.x"}"#),
            _ => Reply::status(404, "{}"),
        })
    });
    let store = memory();
    let service = service(&mock, store.clone(), None);
    let runner = SignInRunner::new(service.clone());
    let phase = sign_in(&runner, SignInMethod::GitLabBrowser, |prompt| {
        let UserPrompt::Browser { url } = prompt else {
            panic!()
        };
        let query = parse_query(url.split_once('?').unwrap().1);
        let get = |k: &str| query.iter().find(|(key, _)| key == k).unwrap().1.clone();
        let redirect = get("redirect_uri");
        let local = redirect
            .strip_prefix("http://")
            .unwrap()
            .strip_suffix("/callback")
            .unwrap();
        browser_get(local, &format!("/callback?code=c&state={}", get("state")));
    });
    match phase {
        SignInPhase::Failed(SignInFailure::Error(message)) => {
            assert!(message.contains("unusable user name"), "{message}");
            assert!(!message.contains("REFRESH1") && !message.contains("refresh1"));
        }
        other => panic!("{other:?}"),
    }
    assert!(store.is_empty());
    assert!(service.accounts().is_empty());
}

#[test]
fn a_cancel_between_the_user_lookup_and_the_commit_stores_nothing() {
    // Cancel lands after the flow's "cancelled?" check and before it
    // commits: the commit itself must still refuse.
    let slot: Arc<Mutex<Option<Arc<SignInRunner>>>> = Arc::default();
    let hook = slot.clone();
    let polls = Arc::new(AtomicUsize::new(0));
    let mock = Mock::start(move |port| {
        Box::new(move |request| match request.path() {
            "/login/device/code" => Reply::json(&format!(
                r#"{{"device_code":"d","user_code":"AAAA-BBBB",
                    "verification_uri":"http://127.0.0.1:{port}/device","expires_in":900,"interval":5}}"#
            )),
            "/login/oauth/access_token" => {
                polls.fetch_add(1, Ordering::SeqCst);
                Reply::json(r#"{"access_token":"ghp_ANA","token_type":"bearer"}"#)
            }
            "/user" => {
                if let Some(runner) = hook.lock().unwrap().as_ref() {
                    abort(runner);
                }
                Reply::json(r#"{"login":"ana"}"#)
            }
            _ => Reply::status(404, "{}"),
        })
    });
    let store = memory();
    let service = service(&mock, store.clone(), None);
    let runner = Arc::new(SignInRunner::new(service.clone()));
    *slot.lock().unwrap() = Some(runner.clone());
    sign_in_until(
        &runner,
        SignInMethod::GitHubDevice,
        |_| {},
        || !mock.requests_to("/user").is_empty(),
    );
    thread::sleep(Duration::from_millis(300));
    assert!(store.is_empty());
    assert!(service.accounts().is_empty());
}

// -- cancel never waits for a write, and never hides a failed undo --------------

#[test]
fn a_cancel_from_inside_the_keychain_write_neither_deadlocks_nor_keeps_the_account() {
    // The store calls Cancel synchronously from its own save, on the thread
    // that holds the write: Cancel must not wait for that write.
    let mock = device_mock("granted");
    let data = TempDir::new("cancel-same-thread");
    let store = Arc::new(HookStore::default());
    let service = service(&mock, store.clone(), Some(data.book()));
    let runner = Arc::new(SignInRunner::new(service.clone()));
    let fired = Arc::new(AtomicBool::new(false));
    {
        let (runner, fired) = (runner.clone(), fired.clone());
        *store.after_save.lock().unwrap() = Some(Box::new(move || {
            abort(&runner);
            fired.store(true, Ordering::SeqCst);
        }));
    }
    sign_in_until(
        &runner,
        SignInMethod::GitHubDevice,
        |_| {},
        || fired.load(Ordering::SeqCst),
    );
    assert!(fired.load(Ordering::SeqCst), "the token was never saved");
    eventually("the write never finished (deadlock?) or was kept", || {
        store.inner.is_empty() && service.accounts().is_empty()
    });
    if let Ok(text) = std::fs::read_to_string(data.book()) {
        assert!(AccountBook::decode(&text).is_empty());
    }
}

/// Reads events until a `CleanupFailed` comes (up to 5 s).
fn wait_for_cleanup_failed(inbox: &mpsc::Receiver<SignInEvent>) -> Option<String> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if let Ok(SignInEvent::CleanupFailed(message)) =
            inbox.recv_timeout(Duration::from_millis(25))
        {
            return Some(message);
        }
    }
    None
}

#[test]
fn a_cancel_whose_undo_fails_says_so_instead_of_staying_silent() {
    let mock = device_mock("granted");
    let store = Arc::new(HookStore::default());
    let service = service(&mock, store.clone(), None);
    let runner = SignInRunner::new(service.clone());
    run_until_committed(&runner, &service);
    store.fail_delete.store(true, Ordering::SeqCst);
    let (events, inbox) = mpsc::channel();
    runner.execute(SignInCommand::Abort, events);
    let message = wait_for_cleanup_failed(&inbox).expect("the failed undo was not reported");
    assert!(message.contains("keychain"), "{message}");
    assert!(message.contains("Invoke Git forge accounts"), "{message}");
    for secret in ["ghp_ANA", "dev-secret-code"] {
        assert!(!message.contains(secret), "{message}");
    }
    // The leftover is real and said so: the account is gone, the token is not.
    assert!(service.accounts().is_empty());
    assert!(!store.inner.is_empty());
    // The machine shows it on the cancelled screen.
    let (phase, _) = step_sign_in(
        SignInPhase::Failed(SignInFailure::Cancelled),
        SignInEvent::CleanupFailed(message),
    );
    assert!(matches!(
        phase,
        SignInPhase::Failed(SignInFailure::CancelledWithLeftover(_))
    ));
}

#[test]
fn a_failed_undo_of_a_write_cancelled_midway_is_reported_by_the_flow() {
    let mock = device_mock("granted");
    let store = Arc::new(HookStore::default());
    let service = service(&mock, store.clone(), None);
    let runner = Arc::new(SignInRunner::new(service.clone()));
    store.fail_delete.store(true, Ordering::SeqCst);
    {
        let runner = runner.clone();
        *store.after_save.lock().unwrap() = Some(Box::new(move || abort(&runner)));
    }
    let (events, inbox) = mpsc::channel();
    let (mut phase, commands) = step_sign_in(
        SignInPhase::Idle,
        SignInEvent::Begin(SignInMethod::GitHubDevice),
    );
    for command in commands {
        runner.execute(command, events.clone());
    }
    let started = Instant::now();
    let mut message = None;
    while message.is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "no report of the failed undo"
        );
        match inbox.recv_timeout(Duration::from_millis(25)) {
            Ok(SignInEvent::CleanupFailed(text)) => message = Some(text),
            Ok(event) => {
                let (next, commands) = step_sign_in(phase, event);
                phase = next;
                for command in commands {
                    runner.execute(command, events.clone());
                }
            }
            Err(_) => {}
        }
    }
    assert!(message.unwrap().contains("keychain"));
}

#[test]
fn a_cancel_that_cannot_read_the_keychain_reports_a_failed_undo() {
    let mock = device_mock("granted");
    let store = Arc::new(HookStore::default());
    let service = service(&mock, store.clone(), None);
    let runner = SignInRunner::new(service.clone());
    run_until_committed(&runner, &service);
    // The keychain goes away after the commit: whether the token is still
    // the flow's cannot be known, so the undo must not claim success.
    store.fail_load.store(true, Ordering::SeqCst);
    let (events, inbox) = mpsc::channel();
    runner.execute(SignInCommand::Abort, events);
    let message = wait_for_cleanup_failed(&inbox).expect("the unknown outcome was hidden");
    assert!(message.contains("keychain"), "{message}");
    assert!(!message.contains("ghp_ANA"));
    // Nothing was touched without knowing what is there.
    assert_eq!(service.accounts().len(), 1);
    assert!(!store.inner.is_empty());
}

// -- the account list on disk is the source of truth -----------------------------

#[test]
fn two_windows_on_one_account_list_keep_each_others_accounts() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("two-windows");
    let store = memory();
    // Two clone dialogs, each with its own service, both opened on the
    // same (empty) list.
    let first = service(&mock, store.clone(), Some(data.book()));
    let second = service(&mock, store.clone(), Some(data.book()));
    let ana = first
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    let work = second
        .connect_with_token(ForgeKind::GitHub, "", "ghp_WORK")
        .unwrap();
    let on_disk = AccountBook::decode(&std::fs::read_to_string(data.book()).unwrap());
    assert!(
        on_disk.get(&ana.key()).is_some(),
        "the first account was dropped"
    );
    assert!(on_disk.get(&work.key()).is_some());
    assert_eq!(first.accounts().len(), 2);
    assert_eq!(store.len(), 2);
}

#[test]
fn a_list_changed_on_disk_during_a_write_is_not_overwritten() {
    let mock = Mock::start(github_handler);
    let data = TempDir::new("changed-on-disk");
    let store = Arc::new(HookStore::default());
    let service = service(&mock, store.clone(), Some(data.book()));
    let mut theirs = AccountBook::default();
    theirs.add(
        ForgeAccount::new(
            ForgeKind::GitLab,
            invoke_core::forge::Origin::parse("https://git.corp").unwrap(),
            "zed",
            AuthMethod::Pat,
        )
        .unwrap(),
    );
    let text = theirs.encode();
    {
        // Another process writes the list while this one is signing in.
        let (path, text) = (data.book(), text.clone());
        *store.after_save.lock().unwrap() = Some(Box::new(move || {
            std::fs::write(&path, &text).unwrap();
        }));
    }
    let error = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap_err();
    assert!(matches!(error, ForgeError::Disk(_)), "{error:?}");
    // Their list stands, and this sign-in left no token behind.
    assert_eq!(std::fs::read_to_string(data.book()).unwrap(), text);
    assert!(store.inner.is_empty());
}

#[test]
fn a_clone_address_that_carries_the_token_is_never_offered() {
    let mock = Mock::start(|_| {
        Box::new(|request| match request.path() {
            "/user" => Reply::json(r#"{"login":"ana"}"#),
            _ => Reply::json(
                r#"[{"full_name":"a/ok","clone_url":"https://github.com/a/ok.git"},
                    {"full_name":"a/path","clone_url":"https://github.com/GHP_ANA/x.git"},
                    {"full_name":"a/query","clone_url":"https://github.com/a/q.git?t=ghp_ANA"},
                    {"full_name":"a/user","clone_url":"https://ghp_ANA@github.com/a/u.git"}]"#,
            ),
        })
    });
    let service = service(&mock, memory(), None);
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_ANA")
        .unwrap();
    let listing = service.list_repos(&account.key(), 1).unwrap();
    let names: Vec<&str> = listing.repos.iter().map(|r| r.full_name.as_str()).collect();
    assert_eq!(names, ["a/ok"]);
}

#[test]
fn a_cancelled_device_request_that_answers_late_cannot_replace_the_new_one() {
    // The first request for a code hangs until released; meanwhile the user
    // cancels and starts again. The late answer must not become the code
    // the new attempt polls with.
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let mock = Mock::start(move |port| {
        Box::new(move |request| match request.path() {
            "/login/device/code" => {
                let code = if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                    let started = Instant::now();
                    while !gate.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(5)
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    "dev-STALE"
                } else {
                    "dev-FRESH"
                };
                Reply::json(&format!(
                    r#"{{"device_code":"{code}","user_code":"AAAA-BBBB",
                        "verification_uri":"http://127.0.0.1:{port}/device","expires_in":900,"interval":1}}"#
                ))
            }
            _ => Reply::json(r#"{"error":"authorization_pending"}"#),
        })
    });
    let service = service(&mock, memory(), None);
    let runner = SignInRunner::new(service);
    let (events, inbox) = mpsc::channel();
    runner.execute(
        SignInCommand::Request(SignInMethod::GitHubDevice),
        events.clone(),
    );
    eventually("the first request never arrived", || {
        !mock.requests_to("/login/device/code").is_empty()
    });
    abort(&runner);
    runner.execute(
        SignInCommand::Request(SignInMethod::GitHubDevice),
        events.clone(),
    );
    let ready = inbox.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(ready, SignInEvent::PromptReady { .. }),
        "{ready:?}"
    );
    // Now the stale answer lands.
    release.store(true, Ordering::SeqCst);
    thread::sleep(Duration::from_millis(300));
    runner.execute(SignInCommand::PollAfter { secs: 1 }, events);
    eventually("no poll was sent", || {
        !mock.requests_to("/login/oauth/access_token").is_empty()
    });
    let polls = mock.requests_to("/login/oauth/access_token");
    assert_eq!(
        polls[0].form_value("device_code").as_deref(),
        Some("dev-FRESH")
    );
    // And the stale attempt said nothing (only the new poll answered).
    thread::sleep(Duration::from_millis(100));
    for event in inbox.try_iter() {
        assert_eq!(event, SignInEvent::Pending);
    }
}
