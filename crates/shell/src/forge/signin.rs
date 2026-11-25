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
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use invoke_core::forge::{
    Attempt, ForgeKind, Origin, SignInCommand, SignInEvent, SignInMethod, SignInReply, UserPrompt,
};

use super::api::{self, github::DevicePoll};
use super::codec::{Pkce, base64url, parse_query};
use super::error::ForgeError;
use super::random;
use super::secrets::Secret;
use super::service::{FlowControl, ForgeService};

/// The device code of the flow in progress, kept for the polls.
struct DeviceSession {
    /// The attempt this code belongs to. A cancelled or replaced attempt that
    /// finishes late never installs its code, and a poll only ever uses the
    /// code of its own attempt.
    owner: Arc<FlowControl>,
    code: Secret,
    client_id: String,
    deadline: Instant,
}

pub struct SignInRunner {
    service: ForgeService,
    /// The newest attempt and its cancel switch; older ones stay cancelled.
    cancel: Mutex<(Attempt, Arc<FlowControl>)>,
    device: Arc<Mutex<Option<DeviceSession>>>,
}

/// Where one attempt's answers go: every answer is tagged with the attempt,
/// so the machine can drop the ones that come too late.
struct Outbox {
    attempt: Attempt,
    events: Sender<SignInEvent>,
}

impl Outbox {
    fn post(&self, reply: SignInReply) {
        let _ = self.events.send(SignInEvent::Reply {
            attempt: self.attempt,
            reply,
        });
    }
}

impl SignInRunner {
    pub fn new(service: ForgeService) -> Self {
        Self {
            service,
            cancel: Mutex::new((Attempt(0), Arc::new(FlowControl::default()))),
            device: Arc::default(),
        }
    }

    /// Carries out one command from `step_sign_in`.
    pub fn execute(&self, command: SignInCommand, events: Sender<SignInEvent>) {
        match command {
            SignInCommand::Request { method, attempt } => {
                let cancel = self.fresh_cancel(attempt);
                let service = self.service.clone();
                let device = self.device.clone();
                let out = Outbox { attempt, events };
                thread::spawn(move || {
                    let outcome = match method {
                        SignInMethod::GitHubDevice => {
                            request_device_code(&service, &device, &cancel, &out)
                        }
                        SignInMethod::GitLabBrowser => browser_flow(&service, &cancel, &out),
                    };
                    if let Err(error) = outcome {
                        send(&cancel, &out, failure_reply(&cancel, &error));
                    }
                });
            }
            SignInCommand::PollAfter { secs } => {
                let (attempt, cancel) = self.current_cancel();
                let service = self.service.clone();
                let device = self.device.clone();
                let out = Outbox { attempt, events };
                thread::spawn(move || {
                    let wait = service.config().poll_unit * secs;
                    if sleep_unless_cancelled(wait, &cancel) {
                        return;
                    }
                    let reply = poll_once(&service, &device, &cancel);
                    send(&cancel, &out, reply);
                });
            }
            SignInCommand::Abort => {
                // Flag cancellation synchronously so in-flight polls/writes
                // stop at once, then run any committed-attempt undo off the
                // caller thread so the UI never blocks on the OS lock or
                // Credential Manager.
                let (attempt, cancel) = self.current_cancel();
                let committed = self.service.begin_cancel(&cancel);
                if let Ok(mut device) = self.device.lock() {
                    *device = None;
                }
                if let Some(commit) = committed {
                    let service = self.service.clone();
                    let out = Outbox { attempt, events };
                    thread::spawn(move || match service.undo_commit(&commit) {
                        Ok(()) => out.post(SignInReply::CleanedUp),
                        Err(error) => out.post(SignInReply::CleanupFailed(error.user_message())),
                    });
                }
            }
        }
    }

    /// Retires whatever ran before (what it committed stays) and returns the
    /// control for a new attempt.
    fn fresh_cancel(&self, attempt: Attempt) -> Arc<FlowControl> {
        let mut current = self.cancel.lock().unwrap_or_else(|p| p.into_inner());
        current.1.retire();
        *current = (attempt, Arc::new(FlowControl::default()));
        current.1.clone()
    }

    fn current_cancel(&self) -> (Attempt, Arc<FlowControl>) {
        let current = self.cancel.lock().unwrap_or_else(|p| p.into_inner());
        (current.0, current.1.clone())
    }
}

/// The answer for a failed step. After a cancel, cleanup completion or
/// failure is still reported so the UI refreshes to match disk/keychain.
fn failure_reply(cancel: &FlowControl, error: &ForgeError) -> SignInReply {
    if cancel.is_cancelled() && *error == ForgeError::RollbackFailed {
        SignInReply::CleanupFailed(error.user_message())
    } else if cancel.is_cancelled() && *error == ForgeError::Cancelled {
        SignInReply::CleanedUp
    } else {
        SignInReply::Errored(error.user_message())
    }
}

/// Sends an answer unless the flow was cancelled meanwhile (cleanup outcomes
/// are still sent so the UI refreshes or warns the user).
fn send(cancel: &FlowControl, out: &Outbox, reply: SignInReply) {
    if !cancel.is_cancelled()
        || matches!(
            reply,
            SignInReply::CleanedUp | SignInReply::CleanupFailed(_)
        )
    {
        out.post(reply);
    }
}

/// Sleeps in short slices; true if the flow was cancelled during the wait.
fn sleep_unless_cancelled(wait: Duration, cancel: &FlowControl) -> bool {
    let slice = Duration::from_millis(10).min(wait.max(Duration::from_millis(1)));
    let end = Instant::now() + wait;
    while Instant::now() < end {
        if cancel.is_cancelled() {
            return true;
        }
        thread::sleep(slice.min(end.saturating_duration_since(Instant::now())));
    }
    cancel.is_cancelled()
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
    cancel: &Arc<FlowControl>,
    out: &Outbox,
) -> Result<(), ForgeError> {
    let client_id = client_id(service, ForgeKind::GitHub)?;
    let code =
        api::github::request_device_code(service.http(), &service.config().github, &client_id)?;
    let session = DeviceSession {
        owner: cancel.clone(),
        code: code.device_code.clone(),
        client_id,
        deadline: Instant::now() + service.config().poll_unit * code.expires_in.min(3600) as u32,
    };
    {
        // Checked under the session lock: Abort and a new attempt flag this
        // attempt before they touch the session, so a stale answer is dropped.
        let mut current = device.lock().unwrap_or_else(|p| p.into_inner());
        if cancel.is_cancelled() {
            return Ok(());
        }
        *current = Some(session);
    }
    send(
        cancel,
        out,
        SignInReply::PromptReady {
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
fn poll_once(
    service: &ForgeService,
    device: &Mutex<Option<DeviceSession>>,
    cancel: &FlowControl,
) -> SignInReply {
    let (code, client_id, deadline) = {
        let guard = device.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            Some(session) if std::ptr::eq(Arc::as_ptr(&session.owner), cancel) => (
                session.code.clone(),
                session.client_id.clone(),
                session.deadline,
            ),
            _ => return SignInReply::Errored("The sign-in is no longer running.".into()),
        }
    };
    if Instant::now() >= deadline {
        return SignInReply::Expired;
    }
    let polled = api::github::poll_device_code(
        service.http(),
        &service.config().github,
        &client_id,
        &code,
        service.now(),
    );
    match polled {
        Ok(DevicePoll::Pending) => SignInReply::Pending,
        Ok(DevicePoll::SlowDown) => SignInReply::SlowDown,
        Ok(DevicePoll::Denied) => SignInReply::Denied,
        Ok(DevicePoll::Expired) => SignInReply::Expired,
        Ok(DevicePoll::Granted(credential)) => {
            let origin = match service.origin_for_new(ForgeKind::GitHub, "") {
                Ok(origin) => origin,
                Err(error) => return failure_reply(cancel, &error),
            };
            match service.finish_oauth(ForgeKind::GitHub, origin, credential, cancel) {
                Ok(account) => SignInReply::Granted(account),
                Err(error) => failure_reply(cancel, &error),
            }
        }
        Err(ForgeError::Network | ForgeError::TimedOut) => SignInReply::Pending,
        Err(error) => failure_reply(cancel, &error),
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

/// How long one connection may take to send its request line.
const CONNECTION_LIMIT: Duration = Duration::from_secs(3);

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let text = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\r\n{body}",
        body.len()
    );
    // The answer is a few hundred bytes; a client that does not read it
    // must not hold the flow up.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let _ = stream.write_all(text.as_bytes());
}

fn page(message: &str) -> String {
    format!("<!doctype html><meta charset=utf-8><title>Invoke Git</title><p>{message}</p>")
}

/// Reads the request line of one connection without ever outliving the
/// flow: reads do not block, and cancellation, the connection's own limit
/// and the flow's deadline are checked between them. A client that connects
/// and says nothing therefore cannot keep the port past Cancel or the
/// deadline.
fn request_line(stream: &mut TcpStream, cancel: &FlowControl, flow_end: Instant) -> Option<String> {
    stream.set_nonblocking(true).ok()?;
    let end = (Instant::now() + CONNECTION_LIMIT).min(flow_end);
    let mut buffer = [0u8; 4096];
    let mut filled = 0;
    while filled < buffer.len() {
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => {
                filled += read;
                if buffer[..filled].windows(2).any(|w| w == b"\r\n") {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if cancel.is_cancelled() || Instant::now() >= end {
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
    let text = std::str::from_utf8(&buffer[..filled]).ok()?;
    text.lines().next().map(str::to_owned)
}

fn browser_flow(
    service: &ForgeService,
    cancel: &FlowControl,
    out: &Outbox,
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
        out,
        SignInReply::PromptReady {
            prompt: UserPrompt::Browser { url },
            interval_secs: 0,
        },
    );

    let end = Instant::now() + service.config().browser_timeout;
    let code = loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        if Instant::now() >= end {
            send(cancel, out, SignInReply::Expired);
            return Ok(());
        }
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(_) => {
                thread::sleep(Duration::from_millis(15));
                continue;
            }
        };
        let Some(line) = request_line(&mut stream, cancel, end) else {
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
                send(cancel, out, SignInReply::Denied);
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
    // The port is released as soon as the redirect is in.
    drop(listener);
    if cancel.is_cancelled() {
        return Ok(());
    }

    let credential = api::gitlab::exchange_code(
        service.http(),
        &origin,
        &client_id,
        &redirect_uri,
        &code,
        pkce.verifier(),
        service.now(),
    )?;
    let account = service.finish_oauth(ForgeKind::GitLab, origin, credential, cancel)?;
    send(cancel, out, SignInReply::Granted(account));
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
