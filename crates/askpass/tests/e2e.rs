//! The helper binary against the shell's server, and a whole clone that
//! prompts for credentials. The "remote" is a throwaway HTTP server on
//! 127.0.0.1 that answers 401 to everything; nothing leaves the machine.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use invoke_core::clone::{CloneEvent, ClonePhase, CloneRequest, CloneState, step_clone};
use invoke_core::{AuthAnswer, AuthKind, AuthPrompt, ErrorClass, OpId, OpOutcome};
use shell::askpass::AskpassServer;
use shell::{CloneRunner, Git};

const HELPER: &str = env!("CARGO_BIN_EXE_askpass");

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "invoke-git-askpass-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs the helper the way git does: the prompt as argv[1].
fn run_helper(env: Vec<(OsString, OsString)>, prompt: &str) -> std::process::Child {
    Command::new(HELPER)
        .arg(prompt)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn server() -> (AskpassServer, mpsc::Receiver<AuthPrompt>) {
    let (sender, receiver) = mpsc::channel();
    let sender = std::sync::Mutex::new(sender);
    let server = AskpassServer::start(
        OpId(7),
        Arc::new(move |prompt| {
            let _ = sender.lock().unwrap().send(prompt);
        }),
    )
    .unwrap();
    (server, receiver)
}

#[test]
fn the_answer_reaches_the_helper_stdout() {
    let (server, prompts) = server();
    let child = run_helper(server.env(Path::new(HELPER)), "Password for 'https://h': ");
    let prompt = prompts.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(prompt.kind, AuthKind::Password);
    assert_eq!(prompt.text, "Password for 'https://h': ");
    server.answer(AuthAnswer("s3cret with spaces".into()));
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"s3cret with spaces\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn shutdown_cancels_a_waiting_prompt() {
    let (server, prompts) = server();
    let child = run_helper(server.env(Path::new(HELPER)), "Username for 'x': ");
    prompts.recv_timeout(Duration::from_secs(10)).unwrap();
    server.shutdown();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn a_wrong_token_is_refused_without_a_prompt() {
    let (server, prompts) = server();
    let env = server
        .env(Path::new(HELPER))
        .into_iter()
        .map(|(key, value)| {
            if key == askpass::ENV_TOKEN {
                (key, OsString::from("not-the-token"))
            } else {
                (key, value)
            }
        })
        .collect();
    let output = run_helper(env, "Password: ").wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(prompts.recv_timeout(Duration::from_millis(300)).is_err());
}

#[test]
fn without_a_server_the_helper_fails_quietly() {
    let output = run_helper(Vec::new(), "Password: ")
        .wait_with_output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

/// Answers 401 with a Basic challenge to every request.
fn serve_401(listener: TcpListener) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { return };
        thread::spawn(move || {
            let mut seen = Vec::new();
            let mut buffer = [0u8; 1024];
            while !seen.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => seen.extend_from_slice(&buffer[..read]),
                }
            }
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });
    }
}

/// Drives a real clone through the pure clone machine: events in, commands
/// out, with the "user" answering from `answers` by prompt kind.
fn drive(url: &str, target: &Path, cancel_on_prompt: bool) -> (CloneState, Vec<AuthKind>) {
    let root = scratch("cfg");
    let global = root.join("gitconfig");
    std::fs::write(&global, "").unwrap();
    let env = [
        ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
        ("GIT_CONFIG_GLOBAL", global.into_os_string()),
    ]
    .into_iter()
    .map(|(key, value)| (OsString::from(key), value));
    let runner = CloneRunner::new(Git::detect("git").unwrap(), HELPER).with_env(env);

    let (sender, receiver) = mpsc::channel();
    let mut state = CloneState::default();
    let mut asked = Vec::new();
    let mut pending = vec![CloneEvent::StartRequested(CloneRequest {
        url: url.to_owned(),
        target: target.to_owned(),
    })];
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        for event in pending.drain(..) {
            let (next, commands) = step_clone(state, event);
            state = next;
            for command in commands {
                runner.execute(command, sender.clone());
            }
        }
        if state
            .jobs
            .iter()
            .all(|job| matches!(job.phase, ClonePhase::Ended(_)))
        {
            break;
        }
        let event = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("clone did not finish");
        if let CloneEvent::AuthNeeded(op, prompt) = &event {
            asked.push(prompt.kind);
            pending.push(if cancel_on_prompt {
                CloneEvent::CancelRequested(*op)
            } else {
                let answer = match prompt.kind {
                    AuthKind::Username => "someone",
                    _ => "hunter2",
                };
                CloneEvent::AuthAnswered(*op, AuthAnswer(answer.into()))
            });
        }
        // Secrets never show up in debug output.
        assert!(!format!("{pending:?}").contains("hunter2"));
        pending.insert(0, event);
    }
    let _ = std::fs::remove_dir_all(root);
    (state, asked)
}

#[test]
fn a_wrong_password_prompts_in_app_then_fails_as_auth() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/repo.git",
        listener.local_addr().unwrap().port()
    );
    thread::spawn(move || serve_401(listener));
    let root = scratch("auth");
    let target = root.join("clone");
    let (state, asked) = drive(&url, &target, false);
    assert_eq!(asked, [AuthKind::Username, AuthKind::Password]);
    let ClonePhase::Ended(OpOutcome::Failed(failure)) = &state.jobs[0].phase else {
        panic!("expected a failure, got {:?}", state.jobs[0].phase);
    };
    assert_eq!(failure.class, ErrorClass::Auth, "{}", failure.details);
    assert!(!failure.details.contains("hunter2"));
    assert!(!target.exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn cancelling_the_prompt_cancels_the_clone() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/repo.git",
        listener.local_addr().unwrap().port()
    );
    thread::spawn(move || serve_401(listener));
    let root = scratch("cancel");
    let target = root.join("clone");
    let (state, asked) = drive(&url, &target, true);
    assert_eq!(asked, [AuthKind::Username]);
    assert_eq!(state.jobs[0].phase, ClonePhase::Ended(OpOutcome::Cancelled));
    assert!(!target.exists());
    let _ = std::fs::remove_dir_all(root);
}

/// Task 17: fetch and push prompt through the same server as a clone.
fn network_op_prompts(args: invoke_core::GitArgs) -> (Vec<AuthKind>, OpOutcome) {
    use invoke_core::Event;
    use shell::GitRunner;
    use shell::precondition::{self, RepoPaths};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://127.0.0.1:{}/repo.git",
        listener.local_addr().unwrap().port()
    );
    thread::spawn(move || serve_401(listener));
    let root = scratch("sync");
    let global = root.join("gitconfig");
    std::fs::write(&global, "").unwrap();
    let env: Vec<(OsString, OsString)> = [
        ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
        ("GIT_CONFIG_GLOBAL", global.into_os_string()),
    ]
    .into_iter()
    .map(|(key, value)| (OsString::from(key), value))
    .collect();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    for setup in [
        vec!["init", "-q", "-b", "main"],
        vec!["remote", "add", "origin", url.as_str()],
        // A push needs something to push.
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ],
    ] {
        let status = Command::new("git")
            .args(&setup)
            .current_dir(&repo)
            .envs(env.iter().map(|(key, value)| (key, value)))
            .status()
            .unwrap();
        assert!(status.success());
    }
    let paths = RepoPaths {
        workdir: repo.clone(),
        git_dir: repo.join(".git"),
    };
    let runner = GitRunner::new(Git::detect("git").unwrap(), paths.clone())
        .with_env(env)
        .with_askpass(HELPER);
    let (sender, receiver) = mpsc::channel();
    let op = OpId(3);
    runner.run(op, args, precondition::current(&paths).unwrap(), sender);
    let mut asked = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    let outcome = loop {
        let event = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("the operation did not finish");
        match event {
            Event::AuthNeeded(id, prompt) => {
                assert_eq!(id, op);
                asked.push(prompt.kind);
                let answer = match prompt.kind {
                    AuthKind::Username => "someone",
                    _ => "hunter2",
                };
                runner.answer(op, AuthAnswer(answer.into()));
            }
            Event::OpFinished(id, outcome) => {
                assert_eq!(id, op);
                break outcome;
            }
            _ => {}
        }
    };
    let _ = std::fs::remove_dir_all(root);
    (asked, outcome)
}

#[test]
fn a_fetch_prompts_in_app_then_fails_as_auth() {
    let (asked, outcome) = network_op_prompts(invoke_core::GitArgs::Fetch {
        remote: "origin".into(),
    });
    assert_eq!(asked, [AuthKind::Username, AuthKind::Password]);
    let OpOutcome::Failed(failure) = outcome else {
        panic!("expected a failure, got {outcome:?}");
    };
    assert_eq!(failure.class, ErrorClass::Auth, "{}", failure.details);
    assert!(!failure.details.contains("hunter2"));
}

#[test]
fn a_push_prompts_in_app_then_fails_as_auth() {
    use invoke_core::{BranchName, GitArgs, RemoteBranch};
    let (asked, outcome) = network_op_prompts(GitArgs::Push {
        local: BranchName("main".into()),
        to: RemoteBranch {
            remote: "origin".into(),
            branch: BranchName("main".into()),
        },
        lease: None,
        set_upstream: false,
    });
    assert_eq!(asked, [AuthKind::Username, AuthKind::Password]);
    assert!(matches!(outcome, OpOutcome::Failed(ref f) if f.class == ErrorClass::Auth));
}
