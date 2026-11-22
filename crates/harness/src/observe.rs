//! What the harness looks at: the observable state of a repo, read with
//! plain git and plain file reads (never through the code under test), and
//! the twin-side snapshots the undo model is built from.

use std::collections::BTreeMap;
use std::fs;

use crate::gitdir::RepoDir;

/// Everything compared between the app repo and its twin after every step:
/// `HEAD`, every ref, the index entries, the stash list and the worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    /// The text of `.git/HEAD`: `ref: refs/heads/x` or a bare object id.
    pub head: String,
    /// Every ref except the app's own undo bookkeeping (`refs/invoke-git/`).
    pub refs: BTreeMap<String, String>,
    /// Every ref, bookkeeping included.
    pub all_refs: BTreeMap<String, String>,
    /// `git ls-files -s`: mode, blob, stage and path of every entry.
    pub index: String,
    /// The stash reflog, byte for byte (ids, messages, order).
    pub stash_log: Vec<u8>,
    pub worktree: BTreeMap<String, Vec<u8>>,
}

pub fn observe(dir: &RepoDir) -> Observed {
    let head = fs::read_to_string(dir.paths.git_dir.join("HEAD"))
        .unwrap_or_default()
        .trim()
        .to_owned();
    let all_refs = all_refs(dir);
    let refs = all_refs
        .iter()
        .filter(|(name, _)| !name.starts_with("refs/invoke-git/"))
        .map(|(name, oid)| (name.clone(), oid.clone()))
        .collect();
    let index = dir
        .git(&["ls-files", "-s", "-z"])
        .stdout
        .split(|byte| *byte == 0)
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    let stash_log =
        fs::read(dir.paths.git_dir.join("logs").join("refs").join("stash")).unwrap_or_default();
    Observed {
        head,
        refs,
        all_refs,
        index,
        stash_log,
        worktree: dir.worktree(),
    }
}

/// Every ref name and the object it names, from `git for-each-ref`.
pub fn all_refs(dir: &RepoDir) -> BTreeMap<String, String> {
    dir.git(&["for-each-ref", "--format=%(refname) %(objectname)"])
        .text()
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(name, oid)| (name.to_owned(), oid.to_owned()))
        .collect()
}

impl Observed {
    /// Paths with an index entry at a conflict stage (1 to 3).
    pub fn has_unmerged(&self) -> bool {
        self.index.lines().any(|entry| {
            entry
                .split('\t')
                .next()
                .and_then(|meta| meta.split(' ').nth(2))
                .is_some_and(|stage| stage != "0")
        })
    }
}

/// Describes the first difference between the app repo and the twin, or
/// `None` when they match.
pub fn first_difference(app: &Observed, twin: &Observed) -> Option<String> {
    if app.head != twin.head {
        return Some(format!(
            "HEAD differs\n  app:  {}\n  twin: {}",
            app.head, twin.head
        ));
    }
    if app.refs != twin.refs {
        let mut lines = Vec::new();
        for name in app.refs.keys().chain(twin.refs.keys()) {
            let (a, t) = (app.refs.get(name), twin.refs.get(name));
            if a != t
                && !lines
                    .iter()
                    .any(|line: &String| line.starts_with(name.as_str()))
            {
                lines.push(format!("{name}: app {a:?}, twin {t:?}"));
            }
        }
        return Some(format!("refs differ\n  {}", lines.join("\n  ")));
    }
    if app.index != twin.index {
        return Some(format!(
            "the index differs\n  app:\n{}\n  twin:\n{}",
            indent(&app.index),
            indent(&twin.index)
        ));
    }
    if app.stash_log != twin.stash_log {
        return Some(format!(
            "the stash list differs\n  app:\n{}\n  twin:\n{}",
            indent(&String::from_utf8_lossy(&app.stash_log)),
            indent(&String::from_utf8_lossy(&twin.stash_log))
        ));
    }
    if app.worktree != twin.worktree {
        let mut lines = Vec::new();
        for name in app.worktree.keys().chain(twin.worktree.keys()) {
            let (a, t) = (app.worktree.get(name), twin.worktree.get(name));
            if a != t
                && !lines
                    .iter()
                    .any(|line: &String| line.starts_with(name.as_str()))
            {
                lines.push(format!(
                    "{name}: app {:?}, twin {:?}",
                    a.map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
                    t.map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
                ));
            }
        }
        return Some(format!("worktree files differ\n  {}", lines.join("\n  ")));
    }
    None
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What `HEAD` is on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadAt {
    Branch(String),
    Detached,
}

/// The twin's refs and index as they were at one moment: what the model of
/// undo saves before an operation and compares after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snap {
    pub head: HeadAt,
    /// The commit `HEAD` is on.
    pub head_oid: String,
    pub refs: BTreeMap<String, String>,
    /// `git write-tree` of the index, or `unmerged`.
    pub index_tree: String,
}

impl Snap {
    pub fn of(observed: &Observed, index_tree: String) -> Self {
        let (head, head_oid) = match observed.head.strip_prefix("ref: ") {
            Some(name) => (
                HeadAt::Branch(name.trim_start_matches("refs/heads/").to_owned()),
                observed
                    .refs
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| "unborn".into()),
            ),
            None => (HeadAt::Detached, observed.head.clone()),
        };
        Self {
            head,
            head_oid,
            refs: observed.refs.clone(),
            index_tree,
        }
    }

    /// The value of a ref by name; `HEAD` is the detached head.
    pub fn value(&self, name: &str) -> Option<&String> {
        if name == "HEAD" {
            return matches!(self.head, HeadAt::Detached).then_some(&self.head_oid);
        }
        self.refs.get(name)
    }
}

/// The names of the refs whose value differs between two snapshots (`HEAD`
/// stands for a detached head).
pub fn footprint(before: &Snap, after: &Snap) -> Vec<String> {
    let mut names: Vec<String> = before
        .refs
        .keys()
        .chain(after.refs.keys())
        .cloned()
        .chain(["HEAD".to_owned()])
        .filter(|name| before.value(name) != after.value(name))
        .collect();
    names.sort();
    names.dedup();
    names
}
