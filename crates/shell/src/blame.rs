//! Blame and file history (P5): who wrote each line of a file, and the
//! commits that touched one file, rename-aware.
//!
//! Both are read-only runs of the system git (`git blame --porcelain`,
//! `git log --follow`), a documented fallback (`docs/DECISIONS.md`): gix
//! has no rename-following blame or `--follow` log, and git's own answer
//! is the oracle. They never take a lock (`GIT_OPTIONAL_LOCKS=0`), run on
//! their own thread and stop when `cancel` is set: the child is killed, so
//! moving to another file leaves nothing running and never blocks the UI.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::Duration;

use crate::git::{GLOBAL_ARGS, Git};
use crate::precondition::RepoPaths;

/// The id git prints for lines that are not committed yet.
const UNCOMMITTED: &str = "0000000000000000000000000000000000000000";

/// One line of the file, with the commit that last changed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameLine {
    /// 1-based line number in the blamed version.
    pub line: u32,
    /// 1-based line number in the commit that introduced it.
    pub original_line: u32,
    /// Full hex id; all zeros when the line is not committed yet.
    pub commit: String,
    pub author: String,
    pub email: String,
    /// Author time and the author's UTC offset, in seconds.
    pub seconds: i64,
    pub offset: i32,
    pub summary: String,
    /// The file's name in that commit (differs after a rename).
    pub path: String,
    /// The line belongs to the first commit git looked at (shallow clone
    /// boundary or `--since` limit).
    pub boundary: bool,
    /// The line ended in CRLF; `text` has the `\r` removed.
    pub crlf: bool,
    pub text: String,
}

impl BlameLine {
    pub fn is_committed(&self) -> bool {
        self.commit != UNCOMMITTED
    }
}

/// One commit of a file's history, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileCommit {
    pub id: String,
    pub author: String,
    pub email: String,
    pub seconds: i64,
    pub offset: i32,
    pub summary: String,
    /// The file's name in this commit (follows renames).
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// `cancel` was set; nothing is wrong.
    Cancelled,
    Failed(String),
}

pub enum BlameEvent {
    Done(Vec<BlameLine>),
    Failed(String),
}

pub enum FileHistoryEvent {
    Done(Vec<FileCommit>),
    Failed(String),
}

/// Reads blame and file history of one repo.
#[derive(Clone, Debug)]
pub struct BlameReader {
    git: Git,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
}

impl BlameReader {
    pub fn new(git: Git, paths: RepoPaths) -> Self {
        Self {
            git,
            paths,
            env: Vec::new(),
        }
    }

    /// Extra environment for every run (tests shut user config out).
    pub fn with_env(mut self, env: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        self.env = env.into_iter().collect();
        self
    }

    /// Blames `path` as of `rev` (`None`: the working tree file, so
    /// uncommitted lines show up). Runs on its own thread; the channel
    /// closes without an event when `cancel` is set.
    pub fn blame(
        &self,
        rev: Option<String>,
        path: String,
        cancel: Arc<AtomicBool>,
    ) -> Receiver<BlameEvent> {
        let (tx, rx) = channel();
        let this = self.clone();
        spawn_job("blame", &tx, move || {
            match this.blame_blocking(rev.as_deref(), &path, &cancel) {
                Ok(lines) => Some(BlameEvent::Done(lines)),
                Err(ReadError::Cancelled) => None,
                Err(ReadError::Failed(e)) => Some(BlameEvent::Failed(e)),
            }
        });
        rx
    }

    /// The commits that touched `path`, from `rev` (`None`: `HEAD`) back,
    /// following renames. `limit` caps the count.
    pub fn file_history(
        &self,
        rev: Option<String>,
        path: String,
        limit: Option<usize>,
        cancel: Arc<AtomicBool>,
    ) -> Receiver<FileHistoryEvent> {
        let (tx, rx) = channel();
        let this = self.clone();
        spawn_job("file history", &tx, move || {
            match this.file_history_blocking(rev.as_deref(), &path, limit, &cancel) {
                Ok(commits) => Some(FileHistoryEvent::Done(commits)),
                Err(ReadError::Cancelled) => None,
                Err(ReadError::Failed(e)) => Some(FileHistoryEvent::Failed(e)),
            }
        });
        rx
    }

    pub fn blame_blocking(
        &self,
        rev: Option<&str>,
        path: &str,
        cancel: &AtomicBool,
    ) -> Result<Vec<BlameLine>, ReadError> {
        check_path(path)?;
        let mut args: Vec<OsString> = vec!["blame".into(), "--porcelain".into()];
        if let Some(rev) = rev {
            args.push(checked_rev(rev)?.into());
        }
        args.extend(["--".into(), path.into()]);
        let out = self.run(args, cancel)?;
        parse_blame(&out).map_err(ReadError::Failed)
    }

    pub fn file_history_blocking(
        &self,
        rev: Option<&str>,
        path: &str,
        limit: Option<usize>,
        cancel: &AtomicBool,
    ) -> Result<Vec<FileCommit>, ReadError> {
        check_path(path)?;
        let mut args: Vec<OsString> = vec![
            "log".into(),
            "--follow".into(),
            "--no-color".into(),
            "--date=raw".into(),
            "--name-only".into(),
            format!("--format={RECORD}%H{UNIT}%an{UNIT}%ae{UNIT}%ad{UNIT}%s{UNIT}").into(),
        ];
        if let Some(limit) = limit {
            args.push(format!("--max-count={limit}").into());
        }
        if let Some(rev) = rev {
            args.push(checked_rev(rev)?.into());
        }
        args.extend(["--".into(), path.into()]);
        let out = self.run(args, cancel)?;
        parse_log(&out).map_err(ReadError::Failed)
    }

    /// Runs git and collects stdout, killing the child when `cancel` is set.
    fn run(&self, args: Vec<OsString>, cancel: &AtomicBool) -> Result<Vec<u8>, ReadError> {
        let mut command: Command = self.git.command(&self.paths.workdir);
        command
            .args(GLOBAL_ARGS)
            .args(&args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| ReadError::Failed(format!("start git: {e}")))?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let (tx, rx) = channel();
        let err_reader = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });
        thread::spawn(move || {
            let mut buf = Vec::new();
            let result = stdout.read_to_end(&mut buf).map(|_| buf);
            let _ = tx.send(result);
        });
        let out = loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ReadError::Cancelled);
            }
            match rx.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => break result,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ReadError::Failed("git output reader stopped".into()));
                }
            }
        };
        let status = child
            .wait()
            .map_err(|e| ReadError::Failed(format!("wait for git: {e}")))?;
        let err = err_reader.join().unwrap_or_default();
        let out = out.map_err(|e| ReadError::Failed(format!("read git output: {e}")))?;
        if cancel.load(Ordering::Relaxed) {
            return Err(ReadError::Cancelled);
        }
        if !status.success() {
            return Err(ReadError::Failed(
                String::from_utf8_lossy(&err).trim().to_owned(),
            ));
        }
        Ok(out)
    }
}

fn spawn_job<T: FailedEvent + Send + 'static>(
    name: &str,
    tx: &std::sync::mpsc::Sender<T>,
    job: impl FnOnce() -> Option<T> + Send + 'static,
) {
    let tx2 = tx.clone();
    let spawned = thread::Builder::new().name(name.into()).spawn(move || {
        if let Some(event) = job() {
            let _ = tx2.send(event);
        }
    });
    if let Err(error) = spawned {
        let _ = tx.send(T::failed(format!("start {name}: {error}")));
    }
}

trait FailedEvent {
    fn failed(message: String) -> Self;
}

impl FailedEvent for BlameEvent {
    fn failed(message: String) -> Self {
        Self::Failed(message)
    }
}

impl FailedEvent for FileHistoryEvent {
    fn failed(message: String) -> Self {
        Self::Failed(message)
    }
}

fn check_path(path: &str) -> Result<(), ReadError> {
    if path.is_empty() || path.contains('\0') {
        return Err(ReadError::Failed(format!("not a file path: {path:?}")));
    }
    Ok(())
}

/// A revision goes before `--`, so it must not look like an option.
fn checked_rev(rev: &str) -> Result<&str, ReadError> {
    if rev.is_empty() || rev.starts_with('-') || rev.contains('\0') {
        return Err(ReadError::Failed(format!("not a revision: {rev:?}")));
    }
    Ok(rev)
}

const RECORD: char = '\u{1e}';
const UNIT: char = '\u{1f}';

fn is_hex_id(s: &str) -> bool {
    s.len() >= 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parses a `+0700` / `-0430` zone into seconds.
fn parse_zone(zone: &str) -> i32 {
    let (sign, digits) = match zone.as_bytes().first() {
        Some(b'-') => (-1, &zone[1..]),
        Some(b'+') => (1, &zone[1..]),
        _ => (1, zone),
    };
    let n: i32 = digits.parse().unwrap_or(0);
    sign * ((n / 100) * 3600 + (n % 100) * 60)
}

#[derive(Default, Clone)]
struct Meta {
    author: String,
    email: String,
    seconds: i64,
    offset: i32,
    summary: String,
    path: String,
    boundary: bool,
}

/// Parses `git blame --porcelain`: a header `<id> <orig> <final> [<n>]`,
/// the commit's metadata the first time it appears, then `\t<line>`.
pub fn parse_blame(out: &[u8]) -> Result<Vec<BlameLine>, String> {
    let mut metas: std::collections::HashMap<String, Meta> = Default::default();
    let mut lines = Vec::new();
    let mut current: Option<(String, u32, u32)> = None;
    for raw in out.split(|&b| b == b'\n') {
        if let Some(content) = raw.strip_prefix(b"\t") {
            let (commit, original_line, line) = current
                .take()
                .ok_or_else(|| "blame: content line without a header".to_owned())?;
            let meta = metas.get(&commit).cloned().unwrap_or_default();
            let (content, crlf) = match content.strip_suffix(b"\r") {
                Some(rest) => (rest, true),
                None => (content, false),
            };
            lines.push(BlameLine {
                line,
                original_line,
                commit,
                author: meta.author,
                email: meta.email,
                seconds: meta.seconds,
                offset: meta.offset,
                summary: meta.summary,
                path: meta.path,
                boundary: meta.boundary,
                crlf,
                text: String::from_utf8_lossy(content).into_owned(),
            });
            continue;
        }
        let text = String::from_utf8_lossy(raw);
        let mut parts = text.splitn(4, ' ');
        let first = parts.next().unwrap_or("");
        if current.is_none() && is_hex_id(first) {
            let original = parts.next().and_then(|s| s.parse::<u32>().ok());
            let line = parts.next().and_then(|s| s.parse::<u32>().ok());
            let (Some(original), Some(line)) = (original, line) else {
                return Err(format!("blame: bad header {text:?}"));
            };
            metas.entry(first.to_owned()).or_default();
            current = Some((first.to_owned(), original, line));
            continue;
        }
        // Metadata of the commit named by the last header.
        let Some((commit, _, _)) = &current else {
            continue;
        };
        let Some(meta) = metas.get_mut(commit) else {
            continue;
        };
        let (key, value) = text.split_once(' ').unwrap_or((&text, ""));
        match key {
            "author" => meta.author = value.to_owned(),
            "author-mail" => meta.email = value.trim_matches(['<', '>']).to_owned(),
            "author-time" => meta.seconds = value.parse().unwrap_or(0),
            "author-tz" => meta.offset = parse_zone(value),
            "summary" => meta.summary = value.to_owned(),
            "filename" => meta.path = value.to_owned(),
            "boundary" => meta.boundary = true,
            _ => {}
        }
    }
    if current.is_some() {
        return Err("blame: output ended inside a line".into());
    }
    Ok(lines)
}

/// Parses the `git log --follow --name-only` output requested above.
pub fn parse_log(out: &[u8]) -> Result<Vec<FileCommit>, String> {
    let text = String::from_utf8_lossy(out);
    let mut commits = Vec::new();
    for record in text.split(RECORD).filter(|r| !r.trim().is_empty()) {
        let mut fields = record.splitn(6, UNIT);
        let (Some(id), Some(author), Some(email), Some(date), Some(summary), Some(rest)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(format!("log: bad record {record:?}"));
        };
        let (seconds, zone) = date.split_once(' ').unwrap_or((date, "+0000"));
        commits.push(FileCommit {
            id: id.to_owned(),
            author: author.to_owned(),
            email: email.to_owned(),
            seconds: seconds
                .parse()
                .map_err(|_| format!("log: bad date {date:?}"))?,
            offset: parse_zone(zone),
            summary: summary.to_owned(),
            path: rest
                .lines()
                .find(|l| !l.is_empty())
                .unwrap_or("")
                .to_owned(),
        });
    }
    Ok(commits)
}
