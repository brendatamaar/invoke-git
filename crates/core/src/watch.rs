//! Pure parts of the file watcher: which part of the repo a changed path
//! belongs to, and the debounce that coalesces bursts of changes.
//!
//! Nothing here reads the clock or the disk. Paths are compared as text and
//! time is passed in, so both halves are testable without a filesystem.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use crate::event::{Area, ChangeSet};

/// How long the watcher stays quiet before it reports (spec: 50 ms).
pub const QUIET: Duration = Duration::from_millis(50);

/// Where a repo lives, for classifying paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchRoots {
    pub workdir: String,
    /// The git dir holding this worktree's own `HEAD`, `index` and markers.
    pub git_dir: String,
    /// The shared git dir (`refs/`, `packed-refs`, `objects/`).
    pub common_dir: String,
    /// Windows and default macOS volumes: names compare without case, and
    /// `\` separates path components.
    pub case_insensitive: bool,
}

impl WatchRoots {
    pub fn new(workdir: &Path, git_dir: &Path, common_dir: &Path, case_insensitive: bool) -> Self {
        let text = |path: &Path| path.to_string_lossy().into_owned();
        Self {
            workdir: text(workdir),
            git_dir: text(git_dir),
            common_dir: text(common_dir),
            case_insensitive,
        }
    }

    /// Splits `text` into components, dropping empty and `.` parts. On
    /// case-insensitive roots `\` also separates and case is folded.
    fn components(&self, text: &str) -> Vec<String> {
        let sep = |c: char| c == '/' || (self.case_insensitive && c == '\\');
        text.split(sep)
            .filter(|part| !part.is_empty() && *part != ".")
            .map(|part| {
                if self.case_insensitive {
                    part.to_lowercase()
                } else {
                    part.to_owned()
                }
            })
            .collect()
    }

    /// The part of the repo `path` (absolute) belongs to, or `None` for
    /// paths that never need a read: `objects/`, files git writes for its
    /// own bookkeeping (`logs/`, `FETCH_HEAD`, `config`, ...), and anything
    /// outside the repo.
    pub fn classify(&self, path: &Path) -> Option<Area> {
        let path = self.components(&path.to_string_lossy());
        let own = self.components(&self.git_dir);
        let common = self.components(&self.common_dir);
        let workdir = self.components(&self.workdir);
        let ci = self.case_insensitive;

        // The own git dir first: it may sit inside the worktree (`.git`) or
        // inside the common dir (`worktrees/<name>`).
        if let Some(rel) = path.strip_prefix(own.as_slice()) {
            return classify_own(rel, ci).or_else(|| {
                // Shared files live in the own git dir when it is the
                // common dir.
                if own == common {
                    classify_common(rel, ci)
                } else {
                    None
                }
            });
        }
        if let Some(rel) = path.strip_prefix(common.as_slice()) {
            return classify_common(rel, ci);
        }
        let rel = path.strip_prefix(workdir.as_slice())?;
        match rel.first() {
            // The worktree root itself, or this repo's `.git` (dir or file).
            None => None,
            Some(first) if names_match(first, ".git", ci) => None,
            Some(_) => Some(Area::Worktree),
        }
    }
}

fn classify_own(rel: &[String], ci: bool) -> Option<Area> {
    let name = rel.first()?;
    let is = |want: &str| names_match(name, want, ci);
    if is("HEAD") {
        Some(Area::Head)
    } else if is("index") || is("index.lock") {
        // The lock's presence is part of the disk state (`LockState`).
        Some(Area::Index)
    } else if [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
    ]
    .into_iter()
    .any(is)
    {
        Some(Area::InProgress)
    } else {
        None
    }
}

fn classify_common(rel: &[String], ci: bool) -> Option<Area> {
    let name = rel.first()?;
    if names_match(name, "refs", ci) || names_match(name, "packed-refs", ci) {
        return Some(Area::Refs);
    }
    // P10: adding, removing, locking, or switching another linked worktree
    // updates `<common_dir>/worktrees/<name>/{gitdir,locked,HEAD}`. Ignore
    // other worktrees' `index` and `logs` so edits in one tab do not churn
    // unrelated tabs.
    if names_match(name, "worktrees", ci) {
        let relevant = match rel.get(2) {
            None => true,
            Some(entry) => {
                names_match(entry, "gitdir", ci)
                    || names_match(entry, "locked", ci)
                    || names_match(entry, "HEAD", ci)
            }
        };
        return relevant.then_some(Area::Refs);
    }
    None
}

/// `name` is already case-folded when `ci`; `want` is the canonical spelling.
fn names_match(name: &str, want: &str, ci: bool) -> bool {
    if ci {
        name == want.to_lowercase()
    } else {
        name == want
    }
}

/// Coalesces changes: reports the union of everything seen once [`QUIET`]
/// has passed since the last change. `now` is any monotonic offset the
/// caller chooses; it is never read here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Debounce {
    pending: BTreeSet<Area>,
    last: Duration,
}

impl Debounce {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records changes seen at `now`. An empty set is not a change and does
    /// not restart the quiet period.
    pub fn push(&mut self, now: Duration, areas: impl IntoIterator<Item = Area>) {
        let mut any = false;
        for area in areas {
            any = true;
            self.pending.insert(area);
        }
        if any {
            self.last = self.last.max(now);
        }
    }

    /// The pending changes, once `now` is at least [`QUIET`] after the last
    /// one. Takes them: the next call returns `None` until more arrive.
    pub fn poll(&mut self, now: Duration) -> Option<ChangeSet> {
        if self.pending.is_empty() || now < self.last + QUIET {
            return None;
        }
        Some(ChangeSet(std::mem::take(&mut self.pending)))
    }

    /// When `poll` will next return something, if anything is pending.
    pub fn deadline(&self) -> Option<Duration> {
        (!self.pending.is_empty()).then(|| self.last + QUIET)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::ALL_AREAS;
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn unix() -> WatchRoots {
        WatchRoots::new(
            Path::new("/w/repo"),
            Path::new("/w/repo/.git"),
            Path::new("/w/repo/.git"),
            false,
        )
    }

    fn win() -> WatchRoots {
        WatchRoots::new(
            Path::new(r"C:\Work\Repo"),
            Path::new(r"C:\Work\Repo\.git"),
            Path::new(r"C:\Work\Repo\.git"),
            true,
        )
    }

    fn linked() -> WatchRoots {
        WatchRoots::new(
            Path::new("/w/linked"),
            Path::new("/w/main/.git/worktrees/linked"),
            Path::new("/w/main/.git"),
            false,
        )
    }

    fn c(roots: &WatchRoots, path: &str) -> Option<Area> {
        roots.classify(&PathBuf::from(path))
    }

    #[test]
    fn classifies_git_dir_files() {
        let r = unix();
        assert_eq!(c(&r, "/w/repo/.git/HEAD"), Some(Area::Head));
        assert_eq!(c(&r, "/w/repo/.git/index"), Some(Area::Index));
        assert_eq!(c(&r, "/w/repo/.git/index.lock"), Some(Area::Index));
        assert_eq!(c(&r, "/w/repo/.git/packed-refs"), Some(Area::Refs));
        assert_eq!(c(&r, "/w/repo/.git/refs/heads/main"), Some(Area::Refs));
        assert_eq!(c(&r, "/w/repo/.git/MERGE_HEAD"), Some(Area::InProgress));
        assert_eq!(
            c(&r, "/w/repo/.git/rebase-merge/msgnum"),
            Some(Area::InProgress)
        );
        assert_eq!(c(&r, "/w/repo/.git/objects/ab/cdef"), None);
        assert_eq!(c(&r, "/w/repo/.git/logs/HEAD"), None);
        assert_eq!(c(&r, "/w/repo/.git/FETCH_HEAD"), None);
        assert_eq!(c(&r, "/w/repo/.git"), None);
        assert_eq!(c(&r, "/elsewhere/file"), None);
    }

    #[test]
    fn classifies_worktree_files() {
        let r = unix();
        assert_eq!(c(&r, "/w/repo/src/main.rs"), Some(Area::Worktree));
        assert_eq!(c(&r, "/w/repo/HEAD"), Some(Area::Worktree));
        assert_eq!(c(&r, "/w/repo/.gitignore"), Some(Area::Worktree));
        assert_eq!(c(&r, "/w/repo"), None);
    }

    #[test]
    fn linked_worktree_splits_own_and_shared() {
        let r = linked();
        assert_eq!(
            c(&r, "/w/main/.git/worktrees/linked/HEAD"),
            Some(Area::Head)
        );
        assert_eq!(
            c(&r, "/w/main/.git/worktrees/linked/index"),
            Some(Area::Index)
        );
        assert_eq!(
            c(&r, "/w/main/.git/worktrees/linked/rebase-merge/end"),
            Some(Area::InProgress)
        );
        assert_eq!(c(&r, "/w/main/.git/refs/heads/main"), Some(Area::Refs));
        assert_eq!(c(&r, "/w/main/.git/packed-refs"), Some(Area::Refs));
        // The main worktree's HEAD is not ours.
        assert_eq!(c(&r, "/w/main/.git/HEAD"), None);
        assert_eq!(c(&r, "/w/main/.git/objects/pack/x.pack"), None);
        assert_eq!(c(&r, "/w/linked/.git"), None);
        assert_eq!(c(&r, "/w/linked/a.txt"), Some(Area::Worktree));
    }

    #[test]
    fn windows_paths_ignore_case_and_separator() {
        let r = win();
        assert_eq!(c(&r, r"C:\Work\Repo\.git\HEAD"), Some(Area::Head));
        assert_eq!(c(&r, r"c:\work\repo\.GIT\head"), Some(Area::Head));
        assert_eq!(c(&r, r"C:\WORK\repo\.git\Refs\heads\x"), Some(Area::Refs));
        assert_eq!(c(&r, r"C:\Work\Repo\.git\objects\ab\cd"), None);
        assert_eq!(c(&r, r"c:\work\repo\.Git\Objects\ab"), None);
        assert_eq!(c(&r, r"C:\Work\Repo\src\lib.rs"), Some(Area::Worktree));
        assert_eq!(c(&r, "C:/Work/Repo/src/lib.rs"), Some(Area::Worktree));
        assert_eq!(c(&r, r"C:\Work\Repo\.GIT"), None);
    }

    #[test]
    fn unix_paths_are_case_sensitive() {
        let r = unix();
        // `.GIT` is an ordinary worktree folder on a case-sensitive disk.
        assert_eq!(c(&r, "/w/repo/.GIT/HEAD"), Some(Area::Worktree));
        assert_eq!(c(&r, "/W/repo/src/a"), None);
    }

    proptest! {
        // Nothing under `objects/` ever produces an area, whatever follows.
        #[test]
        fn objects_never_classify(
            tail in "[a-zA-Z0-9_./]{0,20}",
            insensitive in any::<bool>(),
            upper in any::<bool>(),
        ) {
            let r = if insensitive { win() } else { unix() };
            let base = if insensitive { r"C:\Work\Repo\.git\" } else { "/w/repo/.git/" };
            let name = if upper && insensitive { "OBJECTS" } else { "objects" };
            let p = format!("{base}{name}/{tail}");
            prop_assert_eq!(r.classify(Path::new(&p)), None);
        }

        // Any path under the worktree (outside `.git`) is a worktree change,
        // with either separator on Windows.
        #[test]
        fn worktree_paths_classify_as_worktree(
            parts in prop::collection::vec("[a-zA-Z0-9_-]{1,8}", 1..5),
            backslash in any::<bool>(),
        ) {
            prop_assume!(parts[0].to_lowercase() != ".git");
            let sep = if backslash { "\\" } else { "/" };
            let p = format!("C:{sep}Work{sep}Repo{sep}{}", parts.join(sep));
            prop_assert_eq!(win().classify(Path::new(&p)), Some(Area::Worktree));
        }

        // Case never matters on case-insensitive roots.
        #[test]
        fn case_insensitive_classification_ignores_case(
            parts in prop::collection::vec("[a-zA-Z_]{1,8}", 1..4),
            git_part in prop::sample::select(vec![
                "HEAD", "index", "index.lock", "packed-refs", "refs/heads/x",
                "MERGE_HEAD", "rebase-merge/end", "objects/ab/cd", "logs/HEAD", "config",
            ]),
            inside_git in any::<bool>(),
        ) {
            let r = win();
            let tail = if inside_git { format!(r".git\{git_part}") } else { parts.join(r"\") };
            let lower = format!(r"c:\work\repo\{}", tail.to_lowercase());
            let upper = format!(r"C:\WORK\REPO\{}", tail.to_uppercase());
            prop_assert_eq!(r.classify(Path::new(&lower)), r.classify(Path::new(&upper)));
        }
    }

    fn areas() -> impl Strategy<Value = Vec<Area>> {
        prop::collection::vec(prop::sample::select(ALL_AREAS.to_vec()), 0..4)
    }

    #[test]
    fn debounce_waits_for_quiet_then_emits_once() {
        let mut d = Debounce::new();
        d.push(ms(100), [Area::Index]);
        assert_eq!(d.poll(ms(149)), None);
        d.push(ms(140), [Area::Head]);
        assert_eq!(d.poll(ms(189)), None);
        let set = d.poll(ms(190)).unwrap();
        assert_eq!(set.0, BTreeSet::from([Area::Index, Area::Head]));
        assert_eq!(d.poll(ms(1000)), None);
        assert_eq!(d.deadline(), None);
    }

    #[test]
    fn empty_push_does_not_restart_the_timer() {
        let mut d = Debounce::new();
        d.push(ms(0), [Area::Refs]);
        d.push(ms(40), []);
        assert!(d.poll(ms(50)).is_some());
    }

    proptest! {
        // A schedule of (gap, areas) pushes, each followed by polls at the
        // given offsets. Checks the three laws against a reference.
        #[test]
        fn debounce_matches_reference(
            steps in prop::collection::vec((0u64..120, areas(), 0u64..120), 1..30),
        ) {
            let mut d = Debounce::new();
            let mut now = 0u64;
            let mut last_change: Option<u64> = None;
            let mut seen: BTreeSet<Area> = BTreeSet::new();
            for (gap, push, poll_gap) in steps {
                now += gap;
                d.push(ms(now), push.clone());
                if !push.is_empty() {
                    last_change = Some(now);
                    seen.extend(push);
                }
                now += poll_gap;
                match d.poll(ms(now)) {
                    Some(set) => {
                        // Never early.
                        prop_assert!(now >= last_change.unwrap() + 50);
                        // Exactly what was seen since the last emission.
                        prop_assert_eq!(&set.0, &seen);
                        prop_assert!(!set.0.is_empty());
                        seen.clear();
                        last_change = None;
                    }
                    None => {
                        if let Some(l) = last_change {
                            prop_assert!(now < l + 50);
                        }
                    }
                }
            }
            // Nothing is lost: after enough quiet the rest comes out.
            let end = ms(now + 50);
            match d.poll(end) {
                Some(set) => prop_assert_eq!(set.0, seen),
                None => prop_assert!(seen.is_empty()),
            }
        }
    }
}
