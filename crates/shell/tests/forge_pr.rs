//! Pull Request & Merge Request integration tests (P9):
//! - Mock HTTP servers for GitHub PRs and GitLab MRs (list, pagination, detail,
//!   review/approval status, create same-repo and cross-repo fork, secret scrubbing)
//! - Pure remote/fork target resolution and refusal cases (no upstream, detached,
//!   cross-host, no account)
//! - Real git CLI `CheckoutPr` tests against a local bare repo with
//!   `refs/pull/<n>/head` and `refs/merge-requests/<n>/head` verifying Law 9,
//!   Law 11, Law 12, dirty worktree refusal, and existing branch refusal.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use invoke_core::forge::{
    AccountBook, ConfiguredRemote, CreatePrInput, ForgeKind, PrFilter, PrState, PrTargetError,
    ReviewStatus, default_pr_branch, pr_fetch_ref, resolve_create_target, resolve_remote_for_list,
    validate_create_pr,
};
use invoke_core::{
    Action, BranchName, Command, ErrorClass, Event, Head, Notice, OpOutcome, PrCheckoutRefusal,
    RefName, Refusal, RepoId, open, step,
};
use shell::forge::json::Json;
use shell::forge::{
    ClientIds, ForgeError, ForgeService, GitHubUrls, MemoryStore, ServiceConfig,
};
use shell::git::Git;
use shell::precondition::RepoPaths;
use shell::read::{self, read_remotes, read_state};
use shell::runner::GitRunner;

// ---------------------------------------------------------------------------
// Tiny local HTTP mock server (std::net only)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
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
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn json(body: impl Into<String>) -> Reply {
        Reply {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.into(),
        }
    }

    fn status(status: u16, body: impl Into<String>) -> Reply {
        Reply {
            status,
            ..Reply::json(body)
        }
    }

    fn with(mut self, name: &str, value: impl Into<String>) -> Reply {
        self.headers.push((name.into(), value.into()));
        self
    }
}

struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn start(handler: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let stop_flag = Arc::clone(&stop);
        let rec_ref = Arc::clone(&recorded);
        let handler = Arc::new(handler);
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                        if let Some(req) = read_http_request(&stream) {
                            rec_ref.lock().unwrap().push(req.clone());
                            let reply = handler(&req);
                            write_http_reply(stream, reply);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Server {
            addr,
            stop,
            recorded,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_http_request(mut stream: &TcpStream) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers = Vec::new();
    let mut content_len = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let name = k.trim().to_ascii_lowercase();
            let val = v.trim().to_owned();
            if name == "content-length" {
                content_len = val.parse().unwrap_or(0);
            }
            headers.push((name, val));
        }
    }
    while buf.len() < header_end + content_len {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[header_end..header_end + content_len]).into_owned();
    Some(Recorded {
        method,
        target,
        headers,
        body,
    })
}

fn write_http_reply(mut stream: TcpStream, reply: Reply) {
    let mut raw = format!(
        "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (k, v) in reply.headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str("\r\n");
    raw.push_str(&reply.body);
    let _ = stream.write_all(raw.as_bytes());
}

fn service_for_github(server: &Server) -> ForgeService {
    let store = Arc::new(MemoryStore::default());
    let config = ServiceConfig {
        github: GitHubUrls {
            api: server.url(),
            login: server.url(),
        },
        client_ids: ClientIds::default(),
        gitlab_loopback_port: 0,
        poll_unit: Duration::from_millis(10),
        gitlab_instance: server.url(),
        browser_timeout: Duration::from_secs(5),
    };
    ForgeService::new(store, config, None).unwrap()
}

fn service_for_gitlab(server: &Server) -> ForgeService {
    service_for_github(server)
}

// ---------------------------------------------------------------------------
// 1. GitHub PR list, pagination, detail & review status, create (same & fork)
// ---------------------------------------------------------------------------

#[test]
fn github_list_detail_reviews_and_create_pr_same_repo_and_fork() {
    let server = Server::start(|req| match (req.method.as_str(), req.target.as_str()) {
        ("GET", "/user") => Reply::json(r#"{"login":"alice"}"#),
        ("GET", t) if t.starts_with("/repos/org/tool/pulls?state=open") && !t.contains("page=2") => {
            let next = format!("<http://{}/repos/org/tool/pulls?state=open&page=2>; rel=\"next\"",
                req.header("host").unwrap_or("127.0.0.1"));
            Reply::json(
                r#"[{
                    "number": 42,
                    "title": "Add PR panel",
                    "body": "Initial body",
                    "state": "open",
                    "draft": false,
                    "html_url": "https://github.com/org/tool/pull/42",
                    "updated_at": "2026-11-25T10:00:00Z",
                    "user": {"login": "bob"},
                    "head": {"ref": "feat-pr", "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "repo": {"full_name": "bob/tool"}},
                    "base": {"ref": "main"}
                }]"#,
            )
            .with("Link", next)
        }
        ("GET", "/repos/org/tool/pulls?state=open&page=2") => Reply::json(
            r#"[{
                "number": 43,
                "title": "WIP refactor",
                "body": "",
                "state": "open",
                "draft": true,
                "html_url": "https://github.com/org/tool/pull/43",
                "updated_at": "2026-11-24T10:00:00Z",
                "user": {"login": "alice"},
                "head": {"ref": "wip", "sha": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "repo": {"full_name": "org/tool"}},
                "base": {"ref": "main"}
            }]"#,
        ),
        ("GET", "/repos/org/tool/pulls/42") => Reply::json(
            r#"{
                "number": 42,
                "title": "Add PR panel",
                "body": "Detailed description",
                "state": "open",
                "draft": false,
                "html_url": "https://github.com/org/tool/pull/42",
                "updated_at": "2026-11-25T10:00:00Z",
                "comments": 3,
                "review_comments": 2,
                "mergeable": true,
                "user": {"login": "bob"},
                "head": {"ref": "feat-pr", "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "repo": {"full_name": "bob/tool"}},
                "base": {"ref": "main"}
            }"#,
        ),
        ("GET", t) if t.starts_with("/repos/org/tool/pulls/42/reviews") => Reply::json(
            r#"[
                {"user": {"login": "carol"}, "state": "CHANGES_REQUESTED"},
                {"user": {"login": "carol"}, "state": "APPROVED"},
                {"user": {"login": "dave"}, "state": "COMMENTED"}
            ]"#,
        ),
        ("POST", "/repos/org/tool/pulls") => {
            let body = Json::parse(&req.body).unwrap();
            let title = body.str_field("title").unwrap();
            let head = body.str_field("head").unwrap();
            let base = body.str_field("base").unwrap();
            let draft = body.get("draft").and_then(Json::as_bool).unwrap_or(false);
            Reply::status(
                201,
                format!(
                    r#"{{
                        "number": 99,
                        "title": "{title}",
                        "body": "Created",
                        "state": "open",
                        "draft": {draft},
                        "html_url": "https://github.com/org/tool/pull/99",
                        "user": {{"login": "alice"}},
                        "head": {{"ref": "{head}", "repo": {{"full_name": "alice/tool"}}}},
                        "base": {{"ref": "{base}"}}
                    }}"#
                ),
            )
        }
        _ => Reply::status(404, r#"{"message":"not found"}"#),
    });

    let service = service_for_github(&server);
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_test_token_123")
        .unwrap();

    // 1. Paginated list
    let listing = service
        .list_prs(&account.key(), "org/tool", PrFilter::Open, 5)
        .unwrap();
    assert_eq!(listing.prs.len(), 2);
    assert_eq!(listing.prs[0].number, 42);
    assert_eq!(listing.prs[0].head_repo.as_deref(), Some("bob/tool"));
    assert_eq!(listing.prs[1].number, 43);
    assert!(listing.prs[1].draft);
    assert!(listing.more.is_none());

    // 2. Detail + review status aggregation (Carol's APPROVED supersedes CHANGES_REQUESTED)
    let detail = service.get_pr_detail(&account.key(), "org/tool", 42).unwrap();
    assert_eq!(detail.review_status, ReviewStatus::Approved);
    assert_eq!(detail.comments_count, Some(5));
    assert_eq!(detail.mergeable, Some(true));

    // 3. Create PR from fork (`alice/tool` -> `org/tool`) sends `head: "alice:feat-x"`
    let remotes = vec![
        ConfiguredRemote {
            name: "origin".into(),
            url: "git@github.com:alice/tool.git".into(),
        },
        ConfiguredRemote {
            name: "upstream".into(),
            url: "https://github.com/org/tool.git".into(),
        },
    ];
    let disk = fake_disk_with_upstream("feat-x", "refs/remotes/origin/feat-x");
    let target = resolve_create_target(&remotes, &disk, None, &service.book()).unwrap();
    assert!(target.is_fork);
    let input = validate_create_pr("My fork PR", "Body text", "main", true, &target).unwrap();
    let created = service.create_pr(&target, &input).unwrap();
    assert_eq!(created.number, 99);
    assert!(created.draft);

    let post_req = server
        .requests()
        .into_iter()
        .find(|r| r.method == "POST" && r.target == "/repos/org/tool/pulls")
        .unwrap();
    assert_eq!(
        post_req.header("authorization"),
        Some("Bearer ghp_test_token_123")
    );
    let sent = Json::parse(&post_req.body).unwrap();
    assert_eq!(sent.str_field("head"), Some("alice:feat-x"));
    assert_eq!(sent.str_field("base"), Some("main"));
    assert_eq!(sent.get("draft").and_then(Json::as_bool), Some(true));
}

// ---------------------------------------------------------------------------
// 2. GitLab MR list (nested subgroup), detail & approvals, create fork MR
// ---------------------------------------------------------------------------

#[test]
fn gitlab_list_detail_approvals_and_create_fork_mr() {
    let server = Server::start(|req| match (req.method.as_str(), req.target.as_str()) {
        ("GET", "/api/v4/user") => Reply::json(r#"{"username":"alice"}"#),
        ("GET", t) if t.starts_with("/api/v4/projects/grp%2Fsub%2Ftool/merge_requests?state=opened") => {
            Reply::json(
                r#"[{
                    "iid": 7,
                    "title": "Fix parser",
                    "description": "Fixes edge case",
                    "state": "opened",
                    "draft": false,
                    "web_url": "https://gitlab.example.com/grp/sub/tool/-/merge_requests/7",
                    "updated_at": "2026-11-25T09:00:00Z",
                    "author": {"username": "bob"},
                    "source_branch": "fix-parser",
                    "target_branch": "main",
                    "sha": "cccccccccccccccccccccccccccccccccccccccc",
                    "user_notes_count": 4,
                    "has_conflicts": false
                }]"#,
            )
        }
        ("GET", "/api/v4/projects/grp%2Fsub%2Ftool/merge_requests/7") => Reply::json(
            r#"{
                "iid": 7,
                "title": "Fix parser",
                "description": "Fixes edge case",
                "state": "opened",
                "draft": false,
                "web_url": "https://gitlab.example.com/grp/sub/tool/-/merge_requests/7",
                "author": {"username": "bob"},
                "source_branch": "fix-parser",
                "target_branch": "main",
                "user_notes_count": 4,
                "has_conflicts": false
            }"#,
        ),
        ("GET", "/api/v4/projects/grp%2Fsub%2Ftool/merge_requests/7/approvals") => {
            Reply::json(r#"{"approved": true, "approved_by": [{"user": {"username": "carol"}}]}"#)
        }
        ("GET", "/api/v4/projects/grp%2Fsub%2Ftool") => Reply::json(r#"{"id": 777}"#),
        ("POST", "/api/v4/projects/alice%2Ftool/merge_requests") => {
            let body = Json::parse(&req.body).unwrap();
            let title = body.str_field("title").unwrap();
            let source = body.str_field("source_branch").unwrap();
            let target = body.str_field("target_branch").unwrap();
            Reply::status(
                201,
                format!(
                    r#"{{
                        "iid": 12,
                        "title": "{title}",
                        "description": "From fork",
                        "state": "opened",
                        "draft": true,
                        "web_url": "https://gitlab.example.com/grp/sub/tool/-/merge_requests/12",
                        "author": {{"username": "alice"}},
                        "source_branch": "{source}",
                        "target_branch": "{target}"
                    }}"#
                ),
            )
        }
        _ => Reply::status(404, r#"{"message":"not found"}"#),
    });

    let service = service_for_gitlab(&server);
    let instance = server.url();
    let account = service
        .connect_with_token(ForgeKind::GitLab, &instance, "glpat_secret_456")
        .unwrap();

    // 1. List MRs on nested subgroup
    let listing = service
        .list_prs(&account.key(), "grp/sub/tool", PrFilter::Open, 2)
        .unwrap();
    assert_eq!(listing.prs.len(), 1);
    assert_eq!(listing.prs[0].number, 7);
    assert_eq!(listing.prs[0].state, PrState::Open);
    assert_eq!(listing.prs[0].mergeable, Some(true));

    // 2. Detail + approvals
    let detail = service
        .get_pr_detail(&account.key(), "grp/sub/tool", 7)
        .unwrap();
    assert_eq!(detail.review_status, ReviewStatus::Approved);

    // 3. Create fork MR: posts to source project `alice/tool` with `target_project_id: 777`
    let remotes = vec![
        ConfiguredRemote {
            name: "origin".into(),
            url: format!("{instance}/alice/tool.git"),
        },
        ConfiguredRemote {
            name: "upstream".into(),
            url: format!("{instance}/grp/sub/tool.git"),
        },
    ];
    let disk = fake_disk_with_upstream("feat-gl", "refs/remotes/origin/feat-gl");
    let target = resolve_create_target(&remotes, &disk, None, &service.book()).unwrap();
    assert!(target.is_fork);
    let input = CreatePrInput {
        title: "Add GitLab MR".into(),
        body: "From fork".into(),
        base_branch: "main".into(),
        draft: true,
    };
    let created = service.create_pr(&target, &input).unwrap();
    assert_eq!(created.number, 12);
    assert!(created.draft);

    let post_req = server
        .requests()
        .into_iter()
        .find(|r| r.method == "POST" && r.target == "/api/v4/projects/alice%2Ftool/merge_requests")
        .unwrap();
    let sent = Json::parse(&post_req.body).unwrap();
    assert_eq!(sent.str_field("title"), Some("Draft: Add GitLab MR"));
    assert_eq!(sent.str_field("source_branch"), Some("feat-gl"));
    assert_eq!(sent.str_field("target_branch"), Some("main"));
    assert_eq!(sent.get("target_project_id").and_then(Json::as_u64), Some(777));
}

// ---------------------------------------------------------------------------
// 3. Secret scrubbing & hostile web_url rejection
// ---------------------------------------------------------------------------

#[test]
fn prs_leaking_token_or_unsafe_web_url_are_dropped() {
    let server = Server::start(|req| match req.target.as_str() {
        "/user" => Reply::json(r#"{"login":"alice"}"#),
        t if t.starts_with("/repos/org/tool/pulls") => Reply::json(
            r#"[
                {
                    "number": 1,
                    "title": "Safe PR",
                    "body": "All good",
                    "state": "open",
                    "html_url": "https://github.com/org/tool/pull/1",
                    "user": {"login": "bob"},
                    "head": {"ref": "a"},
                    "base": {"ref": "main"}
                },
                {
                    "number": 2,
                    "title": "Leaks ghp_TOPSECRET_999 here",
                    "body": "",
                    "state": "open",
                    "html_url": "https://github.com/org/tool/pull/2",
                    "user": {"login": "bob"},
                    "head": {"ref": "b"},
                    "base": {"ref": "main"}
                },
                {
                    "number": 3,
                    "title": "Hostile link",
                    "body": "",
                    "state": "open",
                    "html_url": "javascript:alert(1)",
                    "user": {"login": "bob"},
                    "head": {"ref": "c"},
                    "base": {"ref": "main"}
                }
            ]"#,
        ),
        _ => Reply::status(404, "{}"),
    });

    let service = service_for_github(&server);
    let account = service
        .connect_with_token(ForgeKind::GitHub, "", "ghp_TOPSECRET_999")
        .unwrap();
    let listing = service
        .list_prs(&account.key(), "org/tool", PrFilter::Open, 1)
        .unwrap();
    assert_eq!(listing.prs.len(), 1);
    assert_eq!(listing.prs[0].number, 1);

    // Unauthorized error never leaks token
    let err_msg = ForgeError::Unauthorized.user_message();
    assert!(!err_msg.contains("ghp_TOPSECRET_999"));
}

// ---------------------------------------------------------------------------
// 4. Target resolution refusals (no upstream, detached, no account)
// ---------------------------------------------------------------------------

#[test]
fn target_resolution_refuses_missing_upstream_detached_and_unconnected_host() {
    let book = AccountBook::default();
    let remotes = vec![ConfiguredRemote {
        name: "origin".into(),
        url: "https://github.com/org/tool.git".into(),
    }];
    assert_eq!(
        resolve_remote_for_list(&remotes, None, &book),
        Err(PrTargetError::NoAccount {
            remote: "origin".into(),
            host: "github.com".into()
        })
    );

    let mut disk = fake_disk_with_upstream("feat", "refs/remotes/origin/feat");
    disk.refs.upstreams.clear();
    assert_eq!(
        resolve_create_target(&remotes, &disk, None, &book),
        Err(PrTargetError::NoUpstream {
            branch: "feat".into()
        })
    );

    disk.head = Head::Detached(
        invoke_core::Oid::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
    );
    assert_eq!(
        resolve_create_target(&remotes, &disk, None, &book),
        Err(PrTargetError::NotOnBranch)
    );
}

// ---------------------------------------------------------------------------
// 5. Real git CLI `CheckoutPr` tests (Law 9, Law 11, Law 12, dirty refusal)
// ---------------------------------------------------------------------------

fn run_git(dir: &Path, args: &[&str]) -> String {
    let out = ProcessCommand::new("git")
        .current_dir(dir)
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::AtomicU64;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "invoke-git-p9-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Creates a bare remote repo with `main`, `refs/pull/42/head` (GitHub PR), and
/// `refs/merge-requests/7/head` (GitLab MR), plus a cloned local workdir with
/// `main` and an extra local branch `other`.
fn setup_pr_git_fixture() -> (TempDir, RepoPaths, String, String) {
    let dir = TempDir::new("fixture");
    let bare = dir.path().join("remote.git");
    let seed = dir.path().join("seed");
    let work = dir.path().join("work");
    fs::create_dir_all(&bare).unwrap();
    fs::create_dir_all(&seed).unwrap();

    run_git(&bare, &["init", "--bare", "-b", "main"]);
    run_git(&seed, &["init", "-b", "main"]);
    fs::write(seed.join("readme.txt"), "hello\n").unwrap();
    run_git(&seed, &["add", "."]);
    run_git(&seed, &["commit", "-m", "initial"]);
    run_git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    run_git(&seed, &["push", "-u", "origin", "main"]);

    // GitHub PR #42 commit pushed to refs/pull/42/head
    run_git(&seed, &["switch", "-c", "pr-42-src"]);
    fs::write(seed.join("pr42.txt"), "from github pr 42\n").unwrap();
    run_git(&seed, &["add", "."]);
    run_git(&seed, &["commit", "-m", "PR 42 change"]);
    let pr42_oid = run_git(&seed, &["rev-parse", "HEAD"]);
    run_git(&seed, &["push", "origin", "HEAD:refs/pull/42/head"]);

    // GitLab MR #7 commit pushed to refs/merge-requests/7/head
    run_git(&seed, &["switch", "main"]);
    run_git(&seed, &["switch", "-c", "mr-7-src"]);
    fs::write(seed.join("mr7.txt"), "from gitlab mr 7\n").unwrap();
    run_git(&seed, &["add", "."]);
    run_git(&seed, &["commit", "-m", "MR 7 change"]);
    let mr7_oid = run_git(&seed, &["rev-parse", "HEAD"]);
    run_git(&seed, &["push", "origin", "HEAD:refs/merge-requests/7/head"]);

    // Clone into local workdir and add an unrelated local branch `other`.
    run_git(dir.path(), &["clone", bare.to_str().unwrap(), work.to_str().unwrap()]);
    run_git(&work, &["config", "core.autocrlf", "false"]);
    run_git(&work, &["branch", "other"]);

    let paths = RepoPaths {
        git_dir: work.join(".git"),
        workdir: work,
    };
    (dir, paths, pr42_oid, mr7_oid)
}

fn drive_checkout_pr(
    paths: &RepoPaths,
    remote: &str,
    remote_ref: RefName,
    local_branch: &str,
) -> invoke_core::RepoState {
    let git = Git::detect("git").expect("system git >= 2.40");
    let runner = GitRunner::new(git, paths.clone());
    let (mut state, init_cmds) = open(RepoId {
        root_commit: None,
        remote_url: None,
    });
    let mut queue = init_cmds;
    let (tx, rx) = mpsc::channel();

    // First settle initial read.
    while let Some(cmd) = queue.pop() {
        if let Some(ev) = read::execute(paths, &cmd) {
            let (next, cmds) = step(state, ev);
            state = next;
            queue.extend(cmds);
        }
    }

    // Send CheckoutPrRequested.
    let (next, cmds) = step(
        state,
        Event::CheckoutPrRequested {
            remote: remote.into(),
            remote_ref,
            local_branch: local_branch.into(),
        },
    );
    state = next;
    let mut pending = cmds;
    let deadline = Instant::now() + Duration::from_secs(15);

    loop {
        while !pending.is_empty() {
            let batch = std::mem::take(&mut pending);
            for cmd in batch {
                match cmd {
                    Command::ReadState { .. } => {
                        if let Some(ev) = read::execute(paths, &cmd) {
                            let (next, more) = step(state, ev);
                            state = next;
                            pending.extend(more);
                        }
                    }
                    Command::RunGit { op, args, expect } => {
                        runner.run(op, args, expect, tx.clone());
                    }
                    _ => {}
                }
            }
        }
        if state.ops.running.is_none()
            && state.ops.queued.is_empty()
            && matches!(state.reads.freshness, invoke_core::Freshness::Fresh)
        {
            break;
        }
        assert!(Instant::now() < deadline, "checkout PR timed out");
        if let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) {
            let (next, more) = step(state, ev);
            state = next;
            pending.extend(more);
        }
    }
    state
}

#[test]
fn checkout_github_pr_and_gitlab_mr_preserves_other_branches_and_updates_state() {
    let (_dir, paths, pr42_oid, mr7_oid) = setup_pr_git_fixture();

    // read_remotes reads origin without lock
    let remotes = read_remotes(&paths).unwrap();
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].name, "origin");

    let before = read_state(&paths).unwrap();
    let main_before = before.refs.refs.get(&RefName("refs/heads/main".into())).unwrap().clone();
    let other_before = before.refs.refs.get(&RefName("refs/heads/other".into())).unwrap().clone();

    // 1. Checkout GitHub PR #42 -> `pr/42`
    let gh_ref = pr_fetch_ref(ForgeKind::GitHub, 42).unwrap();
    let gh_branch = default_pr_branch(ForgeKind::GitHub, 42).unwrap();
    let state = drive_checkout_pr(&paths, "origin", gh_ref, &gh_branch.0);
    assert!(
        matches!(
            state.notice,
            Some(Notice::OpEnded {
                outcome: OpOutcome::Succeeded,
                ..
            })
        ),
        "unexpected notice after GitHub PR checkout: {:?}",
        state.notice
    );
    let disk = state.snapshot.disk().unwrap();
    // Law 11: state immediately reflects new PR branch at exact PR commit
    assert_eq!(
        disk.head,
        Head::Branch {
            name: BranchName("pr/42".into()),
            oid: invoke_core::Oid::from_hex(&pr42_oid).unwrap(),
        }
    );
    assert!(paths.workdir.join("pr42.txt").is_file());
    // Law 9: `main` and `other` did not move
    assert_eq!(disk.refs.refs.get(&RefName("refs/heads/main".into())), Some(&main_before));
    assert_eq!(disk.refs.refs.get(&RefName("refs/heads/other".into())), Some(&other_before));

    // 2. Checkout GitLab MR #7 -> `mr/7`
    let gl_ref = pr_fetch_ref(ForgeKind::GitLab, 7).unwrap();
    let gl_branch = default_pr_branch(ForgeKind::GitLab, 7).unwrap();
    let state = drive_checkout_pr(&paths, "origin", gl_ref, &gl_branch.0);
    assert!(matches!(
        state.notice,
        Some(Notice::OpEnded {
            outcome: OpOutcome::Succeeded,
            ..
        })
    ));
    let disk = state.snapshot.disk().unwrap();
    assert_eq!(
        disk.head,
        Head::Branch {
            name: BranchName("mr/7".into()),
            oid: invoke_core::Oid::from_hex(&mr7_oid).unwrap(),
        }
    );
    assert!(paths.workdir.join("mr7.txt").is_file());
    // Law 9: `main`, `other`, and `pr/42` all untouched
    assert_eq!(disk.refs.refs.get(&RefName("refs/heads/main".into())), Some(&main_before));
    assert_eq!(disk.refs.refs.get(&RefName("refs/heads/other".into())), Some(&other_before));
    assert_eq!(
        disk.refs.refs.get(&RefName("refs/heads/pr/42".into())),
        Some(&invoke_core::Oid::from_hex(&pr42_oid).unwrap())
    );

    // 3. Re-checking out existing `pr/42` is refused (never overwrites existing branch)
    let state = drive_checkout_pr(
        &paths,
        "origin",
        pr_fetch_ref(ForgeKind::GitHub, 42).unwrap(),
        "pr/42",
    );
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::CheckoutPr,
            reason: Refusal::PrCheckout(PrCheckoutRefusal::BranchAlreadyExists(BranchName(
                "pr/42".into()
            ))),
        })
    );
}

#[test]
fn checkout_pr_refuses_dirty_worktree_and_index_lock() {
    let (_dir, paths, _pr42_oid, _mr7_oid) = setup_pr_git_fixture();

    // 1. Dirty worktree (modified tracked file) is refused before running git
    fs::write(paths.workdir.join("readme.txt"), "dirty edit\n").unwrap();
    let state = drive_checkout_pr(
        &paths,
        "origin",
        pr_fetch_ref(ForgeKind::GitHub, 42).unwrap(),
        "pr/42",
    );
    assert_eq!(
        state.notice,
        Some(Notice::Refused {
            action: Action::CheckoutPr,
            reason: Refusal::PrCheckout(PrCheckoutRefusal::DirtyWorktree),
        })
    );
    // Clean up worktree edit
    fs::write(paths.workdir.join("readme.txt"), "hello\n").unwrap();

    // 2. Law 12: `index.lock` held by another process is refused by precondition guard
    let lock = paths.git_dir.join("index.lock");
    fs::write(&lock, "locked").unwrap();
    let git = Git::detect("git").unwrap();
    let runner = GitRunner::new(git, paths.clone());
    let expect = shell::precondition::current(&paths).unwrap();
    let (tx, rx) = mpsc::channel();
    runner.run(
        invoke_core::OpId(1),
        invoke_core::GitArgs::CheckoutPr {
            remote: "origin".into(),
            remote_ref: pr_fetch_ref(ForgeKind::GitHub, 42).unwrap(),
            local_branch: BranchName("pr/42".into()),
        },
        expect,
        tx,
    );
    let finished = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    fs::remove_file(&lock).unwrap();
    match finished {
        Event::OpFinished(_, OpOutcome::Failed(failure)) => {
            assert_eq!(failure.class, ErrorClass::PreconditionChanged);
        }
        other => panic!("expected PreconditionChanged failure, got {other:?}"),
    }
}

fn fake_disk_with_upstream(branch: &str, upstream_ref: &str) -> invoke_core::DiskState {
    let mut refs = invoke_core::RefsSnapshot::default();
    refs.upstreams.insert(
        BranchName(branch.into()),
        invoke_core::Upstream {
            name: RefName(upstream_ref.into()),
            tracking: invoke_core::Tracking::Apart { ahead: 0, behind: 0 },
        },
    );
    invoke_core::DiskState {
        head: Head::Branch {
            name: BranchName(branch.into()),
            oid: invoke_core::Oid::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap(),
        },
        phase: invoke_core::Phase::Clean,
        index: invoke_core::IndexId::Absent,
        status: invoke_core::StatusSnapshot::default(),
        refs,
        stashes: Vec::new(),
        lock: invoke_core::LockState::Free,
        advanced: invoke_core::AdvancedSnapshot::default(),
    }
}
