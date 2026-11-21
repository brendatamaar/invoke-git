//! The app's side of `GIT_ASKPASS` / `SSH_ASKPASS`.
//!
//! git and ssh run the `askpass` helper binary whenever they need a
//! credential. The helper connects back to the server here (a named pipe on
//! Windows, a Unix socket in a private 0700 folder elsewhere), sends the
//! prompt and the per-operation token, and waits. The server calls `notify`
//! with an [`AuthPrompt`] (the caller turns it into an `AuthNeeded` event),
//! and [`AskpassServer::answer`] sends the user's answer back. Nothing is
//! written to disk: the only file system node is the socket itself, and
//! answers only ever travel over the pipe and the helper's stdout.
//!
//! Prompts are asked one at a time per server, in the order they arrive.
//! Shutting the server down cancels any prompt still waiting, so the helper
//! exits non-zero and git fails instead of hanging.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::hash::{BuildHasher, Hasher};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use askpass::{ENV_ADDRESS, ENV_TOKEN};
use invoke_core::{AuthAnswer, AuthKind, AuthPrompt, OpId};

use crate::runner::lock;

/// Called (on a server thread) for each prompt that needs the user.
pub type Notify = Arc<dyn Fn(AuthPrompt) + Send + Sync>;

pub struct AskpassServer {
    shared: Arc<Shared>,
    address: String,
    #[cfg(unix)]
    folder: std::path::PathBuf,
}

struct Shared {
    token: String,
    stop: AtomicBool,
    /// Held while one prompt is out, so prompts are asked one at a time.
    turn: Mutex<()>,
    /// Where the answer to the prompt currently out is sent.
    slot: Mutex<Option<Sender<AuthAnswer>>>,
    notify: Notify,
}

impl AskpassServer {
    pub fn start(op: OpId, notify: Notify) -> std::io::Result<Self> {
        let token = new_token(op);
        let shared = Arc::new(Shared {
            token,
            stop: AtomicBool::new(false),
            turn: Mutex::new(()),
            slot: Mutex::new(None),
            notify,
        });
        platform::start(op, shared)
    }

    /// The environment that makes git and ssh use the helper at `program`.
    pub fn env(&self, program: &Path) -> Vec<(OsString, OsString)> {
        let mut env: Vec<(OsString, OsString)> = vec![
            ("GIT_ASKPASS".into(), program.into()),
            ("SSH_ASKPASS".into(), program.into()),
            // OpenSSH 8.4+: use the helper even though there is a terminal
            // or no display.
            ("SSH_ASKPASS_REQUIRE".into(), "force".into()),
            (ENV_ADDRESS.into(), self.address.clone().into()),
            (ENV_TOKEN.into(), self.shared.token.clone().into()),
        ];
        // Older OpenSSH only uses an askpass program when DISPLAY is set.
        if std::env::var_os("DISPLAY").is_none_or(|value| value.is_empty()) {
            env.push(("DISPLAY".into(), "invoke-git".into()));
        }
        env
    }

    /// Delivers the user's answer to the prompt that is waiting. Without a
    /// waiting prompt it does nothing.
    pub fn answer(&self, answer: AuthAnswer) {
        if let Some(slot) = lock(&self.shared.slot).take() {
            let _ = slot.send(answer);
        }
    }

    /// Cancels every waiting and future prompt and stops listening.
    pub fn shutdown(&self) {
        // Order matters: stop first, then drop the slot. A handler that sets
        // its slot after we cleared it sees `stop` and gives up.
        self.shared.stop.store(true, Ordering::SeqCst);
        lock(&self.shared.slot).take();
        platform::shutdown(self);
    }
}

impl Drop for AskpassServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One connection: read the request, check the token, ask, reply.
fn serve<S: Read + Write>(shared: &Shared, mut stream: S) -> S {
    let reply = match askpass::read_request(&mut stream) {
        Ok(request) if request.token == shared.token => ask(shared, request.prompt),
        _ => None,
    };
    let _ = askpass::write_reply(&mut stream, &reply);
    stream
}

fn ask(shared: &Shared, prompt: String) -> Option<String> {
    let _turn = lock(&shared.turn);
    if shared.stop.load(Ordering::SeqCst) {
        return None;
    }
    let (sender, receiver) = mpsc::channel();
    *lock(&shared.slot) = Some(sender);
    if shared.stop.load(Ordering::SeqCst) {
        lock(&shared.slot).take();
        return None;
    }
    (shared.notify)(AuthPrompt {
        kind: classify_prompt(&prompt),
        text: prompt,
    });
    // A dropped sender (shutdown) is a cancel.
    receiver.recv().ok().map(|answer| answer.0)
}

/// What kind of secret a prompt asks for. git says `Username for '...': `
/// and `Password for '...': `; ssh says `Enter passphrase for key '...': `,
/// `user@host's password: ` and `Are you sure you want to continue
/// connecting (yes/no/[fingerprint])? `. An unknown prompt counts as a
/// password, so it is masked rather than shown.
pub fn classify_prompt(prompt: &str) -> AuthKind {
    let lower = prompt.to_ascii_lowercase();
    if lower.contains("(yes/no") || lower.contains("continue connecting") {
        AuthKind::HostKey
    } else if lower.contains("passphrase") || lower.contains("pin for") {
        AuthKind::Passphrase
    } else if lower.contains("password") {
        AuthKind::Password
    } else if lower.contains("username") || lower.contains("login") {
        AuthKind::Username
    } else {
        AuthKind::Password
    }
}

/// 128 bits from the standard library's randomly seeded hasher, mixed with
/// the op and process. It only has to be unguessable by other local users.
fn new_token(op: OpId) -> String {
    let mut token = String::new();
    for _ in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(op.0);
        hasher.write_u32(std::process::id());
        token.push_str(&format!("{:016x}", hasher.finish()));
    }
    token
}

#[cfg(unix)]
mod platform {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;

    use invoke_core::OpId;

    use super::{AskpassServer, Shared, serve};

    pub fn start(op: OpId, shared: Arc<Shared>) -> std::io::Result<AskpassServer> {
        // A short path: socket paths are limited to about 100 bytes.
        let folder = std::env::temp_dir().join(format!(
            "ig-{}-{}-{}",
            std::process::id(),
            op.0,
            &shared.token[..8]
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&folder)?;
        let path = folder.join("s");
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&folder);
                return Err(error);
            }
        };
        listener.set_nonblocking(true)?;
        let accepting = Arc::clone(&shared);
        thread::Builder::new()
            .name("askpass accept".into())
            .spawn(move || {
                while !accepting.stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // Some systems hand out sockets that inherit the
                            // listener's non-blocking mode.
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                            let shared = Arc::clone(&accepting);
                            thread::spawn(move || serve(&shared, stream));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(20)),
                    }
                }
            })?;
        Ok(AskpassServer {
            shared,
            address: path.to_string_lossy().into_owned(),
            folder,
        })
    }

    pub fn shutdown(server: &AskpassServer) {
        let _ = std::fs::remove_dir_all(&server.folder);
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::ptr::null_mut;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::thread;

    use invoke_core::OpId;
    use windows_sys::Win32::Foundation::{
        ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    use super::{AskpassServer, Shared, serve};

    pub fn start(op: OpId, shared: Arc<Shared>) -> std::io::Result<AskpassServer> {
        let name = format!(
            r"\\.\pipe\invoke-git-askpass-{}-{}-{}",
            std::process::id(),
            op.0,
            shared.token
        );
        // The first instance claims the name, so nobody else can squat on it.
        let first = create_instance(&name, true)?;
        let accepting = Arc::clone(&shared);
        let pipe_name = name.clone();
        thread::Builder::new()
            .name("askpass accept".into())
            .spawn(move || {
                let mut current = first;
                loop {
                    let connected = connect(&current);
                    if accepting.stop.load(Ordering::SeqCst) || !connected {
                        break;
                    }
                    // The next instance exists before this client is served,
                    // so a second prompt never finds the pipe missing.
                    let next = create_instance(&pipe_name, false);
                    let shared = Arc::clone(&accepting);
                    thread::spawn(move || {
                        let pipe = serve(&shared, current);
                        // Closing the handle first would discard the reply
                        // the client has not read yet; FlushFileBuffers on a
                        // pipe waits until it has.
                        let _ = pipe.sync_all();
                    });
                    match next {
                        Ok(next) => current = next,
                        Err(_) => break,
                    }
                }
            })?;
        Ok(AskpassServer {
            shared,
            address: name,
        })
    }

    fn create_instance(name: &str, first: bool) -> std::io::Result<File> {
        let wide: Vec<u16> = std::ffi::OsStr::new(name)
            .encode_wide()
            .chain(Some(0))
            .collect();
        let open_mode = PIPE_ACCESS_DUPLEX
            | if first {
                FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                0
            };
        // SAFETY: `wide` is NUL terminated and outlives the call; a null
        // security descriptor means the default one.
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: we own the freshly created handle; the File closes it.
        Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
    }

    /// Blocks until a client connects. A client that connected before this
    /// call reports `ERROR_PIPE_CONNECTED`, which also counts.
    fn connect(pipe: &File) -> bool {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the handle is a valid pipe handle owned by `pipe`; no
        // OVERLAPPED is passed, so the call is synchronous.
        unsafe {
            ConnectNamedPipe(pipe.as_raw_handle() as _, null_mut()) != 0
                || GetLastError() == ERROR_PIPE_CONNECTED
        }
    }

    /// Wakes the accept thread out of its blocking connect.
    pub fn shutdown(server: &AskpassServer) {
        let _ = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&server.address);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_are_classified_by_what_they_ask() {
        use AuthKind::*;
        let cases = [
            ("Username for 'https://example.com': ", Username),
            ("Password for 'https://me@example.com': ", Password),
            (
                "Enter passphrase for key '/home/me/.ssh/id_ed25519': ",
                Passphrase,
            ),
            ("me@example.com's password: ", Password),
            (
                "The authenticity of host 'h (1.2.3.4)' can't be established.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ",
                HostKey,
            ),
            ("something unexpected", Password),
        ];
        for (prompt, kind) in cases {
            assert_eq!(classify_prompt(prompt), kind, "{prompt}");
        }
    }

    #[test]
    fn tokens_differ_between_servers() {
        assert_ne!(new_token(OpId(1)), new_token(OpId(1)));
        assert_eq!(new_token(OpId(1)).len(), 32);
    }
}
