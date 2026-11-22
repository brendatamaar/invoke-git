//! Generated fixture repos and a way to run plain git on them. Nothing here
//! touches the user's real repositories or their git config: system and
//! global config are shut out and identity and dates are fixed, so two repos
//! given the same commands end up with the same object ids.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use shell::precondition::RepoPaths;

/// A temp folder holding the app repo and its twin, deleted on drop.
pub struct Root {
    pub path: PathBuf,
}

impl Root {
    pub fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path = std::env::temp_dir().join(format!(
            "invoke-harness-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&path).expect("create temp root");
        // macOS keeps temp folders behind a /var symlink that git and gix
        // resolve differently; Windows would give a `\\?\` path that Git
        // for Windows rejects in GIT_CONFIG_GLOBAL.
        #[cfg(not(windows))]
        let path = path.canonicalize().expect("canonicalize temp root");
        Self { path }
    }
}

impl Default for Root {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The result of one git run.
pub struct Run {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Run {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }
}

/// One repo (or its twin) and the environment every git run in it gets.
#[derive(Clone)]
pub struct RepoDir {
    pub paths: RepoPaths,
    pub env: Vec<(OsString, OsString)>,
}

/// Every commit in every repo is made at this moment, so the same commit
/// (parents, tree, message) has the same id in the app repo and the twin.
const DATE: &str = "2020-01-01T00:00:00 +0000";

impl RepoDir {
    /// `git init` in `root/<name>`.
    pub fn init(root: &Path, name: &str) -> Self {
        let workdir = root.join(name);
        fs::create_dir_all(&workdir).expect("create repo dir");
        let global = root.join(format!("{name}.gitconfig"));
        fs::write(&global, "").expect("write empty global config");
        let env = [
            ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
            ("GIT_CONFIG_GLOBAL", global.into_os_string()),
            ("GIT_AUTHOR_NAME", "Test".into()),
            ("GIT_AUTHOR_EMAIL", "test@example.com".into()),
            ("GIT_COMMITTER_NAME", "Test".into()),
            ("GIT_COMMITTER_EMAIL", "test@example.com".into()),
            ("GIT_AUTHOR_DATE", DATE.into()),
            ("GIT_COMMITTER_DATE", DATE.into()),
        ]
        .into_iter()
        .map(|(key, value)| (OsString::from(key), value))
        .collect();
        let dir = Self {
            paths: RepoPaths {
                git_dir: workdir.join(".git"),
                workdir,
            },
            env,
        };
        assert!(dir.git(&["init", "-q", "-b", "main"]).ok, "git init failed");
        dir
    }

    /// A copy of this repo (worktree and `.git`) in `root/<name>`.
    pub fn duplicate(&self, root: &Path, name: &str) -> Self {
        let workdir = root.join(name);
        let _ = fs::remove_dir_all(&workdir);
        copy_dir(&self.paths.workdir, &workdir);
        Self {
            paths: RepoPaths {
                git_dir: workdir.join(".git"),
                workdir,
            },
            env: self.env.clone(),
        }
    }

    pub fn git(&self, args: &[&str]) -> Run {
        self.run(args, None)
    }

    pub fn git_stdin(&self, args: &[&str], stdin: &[u8]) -> Run {
        self.run(args, Some(stdin))
    }

    fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Run {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(&self.paths.workdir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            // Looking must not write: no opportunistic index refresh.
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("start git");
        if let (Some(mut pipe), Some(bytes)) = (child.stdin.take(), stdin) {
            let _ = pipe.write_all(bytes);
        }
        let output = child.wait_with_output().expect("wait for git");
        Run {
            ok: output.status.success(),
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        }
    }

    pub fn full(&self, path: &str) -> PathBuf {
        let mut full = self.paths.workdir.clone();
        full.extend(path.split('/'));
        full
    }

    pub fn write(&self, path: &str, content: &str) {
        let full = self.full(path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent folders");
        }
        fs::write(full, content).expect("write file");
    }

    pub fn read(&self, path: &str) -> Option<String> {
        fs::read(self.full(path))
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn remove(&self, path: &str) {
        let _ = fs::remove_file(self.full(path));
        // Leave no empty folders behind: git does not track them either.
        let mut parent = self.full(path);
        while parent.pop() && parent != self.paths.workdir {
            if fs::remove_dir(&parent).is_err() {
                break;
            }
        }
    }

    pub fn rename(&self, from: &str, to: &str) {
        let target = self.full(to);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).expect("create parent folders");
        }
        fs::rename(self.full(from), target).expect("rename file");
        let mut parent = self.full(from);
        while parent.pop() && parent != self.paths.workdir {
            if fs::remove_dir(&parent).is_err() {
                break;
            }
        }
    }

    /// Every file under the working tree, outside `.git`, with its bytes.
    pub fn worktree(&self) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        walk(&self.paths.workdir, "", &mut files);
        files
    }
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create copy folder");
    for entry in fs::read_dir(from).expect("read folder to copy").flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

fn walk(dir: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if prefix.is_empty() && name == ".git" {
            continue;
        }
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(&entry.path(), &path, files),
            Ok(_) => {
                files.insert(path, fs::read(entry.path()).unwrap_or_default());
            }
            Err(_) => {}
        }
    }
}

/// Quotes one argument for a POSIX shell, so a replay script can be pasted.
pub fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Text as a bash `$'...'` string, so CR, LF, tabs and unicode survive.
pub fn dollar_quote(text: &str) -> String {
    let mut out = String::from("$'");
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_keeps_special_characters() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote("--message=wip"), "--message=wip");
        assert_eq!(dollar_quote("a\r\nü\t'"), "$'a\\r\\nü\\t\\''");
    }
}
