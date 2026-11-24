//! Carries out the sign-in machine's commands (`invoke_core::forge`): the
//! GitHub device flow and the GitLab browser flow with PKCE. Every command
//! runs on its own thread and reports back with a `SignInEvent`; a cancelled
//! flow reports nothing more.
//!
//! The runner holds no decisions: when to poll again, how long to wait after
//! `slow_down` and what each answer ends in are the machine's. It only does
//! the IO the machine asks for.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use invoke_core::forge::{ForgeKind, Origin, SignInCommand, SignInEvent, SignInMethod, UserPrompt};

use super::api::{self, github::DevicePoll};
use super::codec::{Pkce, base64url, parse_query};
use super::error::ForgeError;
use super::random;
use super::secrets::Secret;
use super::service::ForgeService;

/// The device code of the flow in progress, kept for the polls.
struct DeviceSession {
    code: Secret,
    client_id: String,
    deadline: Instant,
}

pub struct SignInRunner {
    service: ForgeService,
    /// Cancel switch of the newest flow; older flows stay cancelled.
    cancel: Mutex<Arc<AtomicBool>>,
    device: Arc<Mutex<Option<DeviceSession>>>,
}

impl SignInRunner {
    pub fn new(service: ForgeService) -> Self {
        Self {
            service,
            cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
            device: Arc::default(),
        }
    }

    /// Carries out one command from `step_sign_in`.
    pub fn execute(&self, command: SignInCommand, events: Sender<SignInEvent>) {
        match command {
            SignInCommand::Request(method) => {
                let cancel = self.fresh_cancel();
                let service = self.service.clone();
                let device = self.device.clone();
                thread::spawn(move || {
                    let outcome = match method {
                        SignInMethod::GitHubDevice => {
                            request_device_code(&service, &device, &cancel, &events)
                        }
                        SignInMethod::GitLabBrowser => browser_flow(&service, &cancel, &events),
                    };
                    if let Err(error) = outcome {
                        send(&cancel, &events, SignInEvent::Errored(error.user_message()));
                    }
                });
            }
            SignInCommand::PollAfter { secs } => {
                let cancel = self.current_cancel();
                let service = self.service.clone();
                let device = self.device.clone();
                thread::spawn(move || {
                    let wait = service.config().poll_unit * secs;
                    if sleep_unless_cancelled(wait, &cancel) {
                        return;
                    }
                    let event = poll_once(&service, &device);
                    send(&cancel, &events, event);
                });
            }
            SignInCommand::Abort => {
                self.current_cancel().store(true, Ordering::SeqCst);
                if let Ok(mut device) = self.device.lock() {
                    *device = None;
                }
            }
        }
    }

    /// Cancels whatever ran before and returns the switch for a new flow.
    fn fresh_cancel(&self) -> Arc<AtomicBool> {
        let mut current = self.cancel.lock().unwrap_or_else(|p| p.into_inner());
        current.store(true, Ordering::SeqCst);
        *current = Arc::new(AtomicBool::new(false));
        current.clone()
    }

    fn current_cancel(&self) -> Arc<AtomicBool> {
        self.cancel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Sends an event unless the flow was cancelled meanwhile.
fn send(cancel: &AtomicBool, events: &Sender<SignInEvent>, event: SignInEvent) {
    if !cancel.load(Ordering::SeqCst) {
        let _ = events.send(event);
    }
}

/// Sleeps in short slices; true if the flow was cancelled during the wait.
fn sleep_unless_cancelled(wait: Duration, cancel: &AtomicBool) -> bool {
    let slice = Duration::from_millis(10).min(wait.max(Duration::from_millis(1)));
    let end = Instant::now() + wait;
    while Instant::now() < end {
        if cancel.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(slice.min(end.saturating_duration_since(Instant::now())));
    }
    cancel.load(Ordering::SeqCst)
}

fn client_id(service: &ForgeService, kind: ForgeKind) -> Result<String, ForgeError> {
    let ids = &service.config().client_ids;
    match kind {
        ForgeKind::GitHub => ids.github.clone(),
        ForgeKind::GitLab => ids.gitlab.clone(),
    }
    .ok_or(ForgeError::NotConfigured(kind))
}

// -- GitHub device flow --------------------------------------------------------

fn request_device_code(
    service: &ForgeService,
    device: &Mutex<Option<DeviceSession>>,
    cancel: &AtomicBool,
    events: &Sender<SignInEvent>,
) -> Result<(), ForgeError> {
    let client_id = client_id(service, ForgeKind::GitHub)?;
    let code =
        api::github::request_device_code(service.http(), &service.config().github, &client_id)?;
    let session = DeviceSession {
        code: code.device_code.clone(),
        client_id,
        deadline: Instant::now() + service.config().poll_unit * code.expires_in.min(3600) as u32,
    };
    *device.lock().unwrap_or_else(|p| p.into_inner()) = Some(session);
    send(
        cancel,
        events,
        SignInEvent::PromptReady {
            prompt: UserPrompt::DeviceCode {
                user_code: code.user_code,
                verification_url: code.verification_uri,
            },
            interval_secs: code.interval,
        },
    );
    Ok(())
}

/// One poll, as the event the machine understands. A network blip while
/// polling is "not yet": the code stays valid, so the machine polls again.
fn poll_once(service: &ForgeService, device: &Mutex<Option<DeviceSession>>) -> SignInEvent {
    let (code, client_id, deadline) = {
        let guard = device.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            Some(session) => (
                session.code.clone(),
                session.client_id.clone(),
                session.deadline,
            ),
            None => return SignInEvent::Errored("The sign-in is no longer running.".into()),
        }
    };
    if Instant::now() >= deadline {
        return SignInEvent::Expired;
    }
    let polled = api::github::poll_device_code(
        service.http(),
        &service.config().github,
        &client_id,
        &code,
        service.now(),
    );
    match polled {
        Ok(DevicePoll::Pending) => SignInEvent::Pending,
        Ok(DevicePoll::SlowDown) => SignInEvent::SlowDown,
        Ok(DevicePoll::Denied) => SignInEvent::Denied,
        Ok(DevicePoll::Expired) => SignInEvent::Expired,
        Ok(DevicePoll::Granted(credential)) => {
            let origin = match service.origin_for_new(ForgeKind::GitHub, "") {
                Ok(origin) => origin,
                Err(error) => return SignInEvent::Errored(error.user_message()),
            };
            match service.finish_oauth(ForgeKind::GitHub, origin, credential) {
                Ok(account) => SignInEvent::Granted(account),
                Err(error) => SignInEvent::Errored(error.user_message()),
            }
        }
        Err(ForgeError::Network | ForgeError::TimedOut) => SignInEvent::Pending,
        Err(error) => SignInEvent::Errored(error.user_message()),
    }
}

// -- GitLab browser flow (PKCE) ------------------------------------------------

/// What the browser's request to the loopback listener asked.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Callback {
    /// Not the redirect (a favicon request, a probe): answer 404 and wait on.
    Other,
    /// The redirect, but with a `state` that is not ours: refuse it, wait on.
    WrongState,
    Denied,
    Code(String),
    /// A redirect without a code or an error.
    Malformed,
}

/// Reads the request line of the browser's redirect.
pub(super) fn read_callback(request_line: &str, expected_state: &str) -> Callback {
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Callback::Other;
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/callback" {
        return Callback::Other;
    }
    let query = parse_query(query);
    let field = |name: &str| {
        query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    if field("state") != Some(expected_state) {
        return Callback::WrongState;
    }
    if field("error").is_some() {
        return Callback::Denied;
    }
    match field("code") {
        Some(code) if !code.is_empty() => Callback::Code(code.to_owned()),
        _ => Callback::Malformed,
    }
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let text = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(text.as_bytes());
}

fn page(message: &str) -> String {
    format!("<!doctype html><meta charset=utf-8><title>Invoke Git</title><p>{message}</p>")
}

fn request_line(stream: &mut TcpStream) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buffer = [0u8; 4096];
    let mut filled = 0;
    while filled < buffer.len() {
        let read = stream.read(&mut buffer[filled..]).ok()?;
        if read == 0 {
            break;
        }
        filled += read;
        if buffer[..filled].windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&buffer[..filled]).ok()?;
    text.lines().next().map(str::to_owned)
}

fn browser_flow(
    service: &ForgeService,
    cancel: &AtomicBool,
    events: &Sender<SignInEvent>,
) -> Result<(), ForgeError> {
    let client_id = client_id(service, ForgeKind::GitLab)?;
    let origin = Origin::parse(&service.config().gitlab_instance).map_err(ForgeError::BadOrigin)?;
    let listener = TcpListener::bind(("127.0.0.1", service.config().gitlab_loopback_port))
        .map_err(|_| {
            ForgeError::Unavailable(
                "The sign-in page could not be opened here: its local port is in use. Close the other program or use an access token."
                    .into(),
            )
        })?;
    listener
        .set_nonblocking(true)
        .map_err(|_| ForgeError::Network)?;
    let port = listener
        .local_addr()
        .map_err(|_| ForgeError::Network)?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let random = |why: String| ForgeError::Unavailable(why);
    let pkce = Pkce::from_random(&random::bytes::<32>().map_err(random)?);
    let state = base64url(&random::bytes::<16>().map_err(ForgeError::Unavailable)?);
    let url = api::gitlab::authorize_url(
        &origin,
        &client_id,
        &redirect_uri,
        &state,
        &pkce.challenge(),
    );
    send(
        cancel,
        events,
        SignInEvent::PromptReady {
            prompt: UserPrompt::Browser { url },
            interval_secs: 0,
        },
    );

    let end = Instant::now() + service.config().browser_timeout;
    let code = loop {
        if cancel.load(Ordering::SeqCst) {
            return Ok(());
        }
        if Instant::now() >= end {
            send(cancel, events, SignInEvent::Expired);
            return Ok(());
        }
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(_) => {
                thread::sleep(Duration::from_millis(15));
                continue;
            }
        };
        let Some(line) = request_line(&mut stream) else {
            continue;
        };
        match read_callback(&line, &state) {
            Callback::Other => respond(&mut stream, "404 Not Found", &page("Not found.")),
            Callback::WrongState | Callback::Malformed => respond(
                &mut stream,
                "400 Bad Request",
                &page("This sign-in link does not belong to Invoke Git. Start again from the app."),
            ),
            Callback::Denied => {
                respond(
                    &mut stream,
                    "200 OK",
                    &page("Sign-in was cancelled. You can close this tab."),
                );
                send(cancel, events, SignInEvent::Denied);
                return Ok(());
            }
            Callback::Code(code) => {
                respond(
                    &mut stream,
                    "200 OK",
                    &page("Signed in. You can close this tab and return to Invoke Git."),
                );
                break code;
            }
        }
    };
    drop(listener);

    let credential = api::gitlab::exchange_code(
        service.http(),
        &origin,
        &client_id,
        &redirect_uri,
        &code,
        pkce.verifier(),
        service.now(),
    )?;
    let account = service.finish_oauth(ForgeKind::GitLab, origin, credential)?;
    send(cancel, events, SignInEvent::Granted(account));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_redirect_is_read_only_when_the_state_matches() {
        let line = |q: &str| format!("GET /callback?{q} HTTP/1.1");
        assert_eq!(
            read_callback(&line("code=abc%2F1&state=s1"), "s1"),
            Callback::Code("abc/1".into())
        );
        assert_eq!(
            read_callback(&line("code=abc&state=other"), "s1"),
            Callback::WrongState
        );
        assert_eq!(read_callback(&line("code=abc"), "s1"), Callback::WrongState);
        assert_eq!(
            read_callback(&line("error=access_denied&state=s1"), "s1"),
            Callback::Denied
        );
        assert_eq!(read_callback(&line("state=s1"), "s1"), Callback::Malformed);
        assert_eq!(
            read_callback(&line("code=&state=s1"), "s1"),
            Callback::Malformed
        );
        assert_eq!(
            read_callback("GET /favicon.ico HTTP/1.1", "s1"),
            Callback::Other
        );
        assert_eq!(
            read_callback("POST /callback HTTP/1.1", "s1"),
            Callback::Other
        );
        assert_eq!(read_callback("", "s1"), Callback::Other);
    }
}
