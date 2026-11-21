//! The file watcher: `notify` events in, debounced `Event::DiskChanged` out.
//!
//! Classification and debouncing are pure and live in `invoke_core::watch`.
//! This file only owns the IO: the OS watches, a thread that feeds the
//! debouncer from the real clock, and the ignore-rule lookup through gix.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use invoke_core::watch::{Debounce, WatchRoots};
use invoke_core::{Area, Event};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::discover::Discovered;

#[derive(Debug)]
pub struct WatchError(String);

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot watch the repository: {}", self.0)
    }
}

impl std::error::Error for WatchError {}

/// Watches one repo until dropped. Every quiet period after a change that
/// matters sends one `Event::DiskChanged` on the channel given to [`start`].
///
/// [`start`]: RepoWatcher::start
pub struct RepoWatcher {
    // Dropped first: closing the raw-event channel stops the thread.
    watcher: Option<RecommendedWatcher>,
    thread: Option<JoinHandle<()>>,
}

impl RepoWatcher {
    /// Starts watching the worktree and the git dirs of `repo`. Returns once
    /// the OS watches are in place, so changes made afterwards are seen.
    pub fn start(repo: &Discovered, events: Sender<Event>) -> Result<Self, WatchError> {
        let case_insensitive = cfg!(any(windows, target_os = "macos"));
        let roots = WatchRoots::new(
            &repo.paths.workdir,
            &repo.paths.git_dir,
            &repo.common_dir,
            case_insensitive,
        );
        let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<notify::Event>>();
        let mut watcher = notify::recommended_watcher(move |result| {
            let _ = raw_tx.send(result);
        })
        .map_err(|e| WatchError(e.to_string()))?;

        // The git dirs usually sit inside the worktree; watch them on their
        // own only when they do not.
        let workdir = &repo.paths.workdir;
        let mut targets: Vec<&Path> = vec![workdir];
        for dir in [&repo.paths.git_dir, &repo.common_dir] {
            if !dir.starts_with(workdir) && !targets.iter().any(|t| dir.starts_with(t)) {
                targets.push(dir);
            }
        }
        for target in targets {
            watcher
                .watch(target, RecursiveMode::Recursive)
                .map_err(|e| WatchError(format!("{}: {e}", target.display())))?;
        }

        let ignore = IgnoreCheck::new(&repo.paths.git_dir, workdir);
        let thread = thread::Builder::new()
            .name("invoke-watcher".into())
            .spawn(move || run(roots, ignore, raw_rx, events))
            .map_err(|e| WatchError(e.to_string()))?;
        Ok(Self {
            watcher: Some(watcher),
            thread: Some(thread),
        })
    }
}

impl Drop for RepoWatcher {
    fn drop(&mut self) {
        self.watcher.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    roots: WatchRoots,
    ignore: IgnoreCheck,
    raw: mpsc::Receiver<notify::Result<notify::Event>>,
    out: Sender<Event>,
) {
    let origin = Instant::now();
    let mut debounce = Debounce::new();
    loop {
        let wait = match debounce.deadline() {
            Some(deadline) => deadline.saturating_sub(origin.elapsed()),
            // Nothing pending: sleep until something arrives.
            None => Duration::from_secs(3600),
        };
        match raw.recv_timeout(wait) {
            Ok(result) => {
                // Take everything already queued, so one ignore lookup
                // session covers the burst.
                let mut batch = vec![result];
                batch.extend(raw.try_iter());
                let now = origin.elapsed();
                debounce.push(now, areas_of(&roots, &ignore, batch));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if let Some(changes) = debounce.poll(origin.elapsed())
            && out.send(Event::DiskChanged(changes)).is_err()
        {
            return;
        }
    }
}

fn areas_of(
    roots: &WatchRoots,
    ignore: &IgnoreCheck,
    batch: Vec<notify::Result<notify::Event>>,
) -> Vec<Area> {
    let mut lookup = ignore.session();
    let mut areas = Vec::new();
    for result in batch {
        let Ok(event) = result else {
            // The OS dropped events (overflow) or a watch broke: we cannot
            // know what changed, so read everything.
            areas.extend(invoke_core::event::ALL_AREAS);
            continue;
        };
        if matches!(event.kind, EventKind::Access(_)) {
            continue;
        }
        if event.need_rescan() {
            areas.extend(invoke_core::event::ALL_AREAS);
            continue;
        }
        for path in &event.paths {
            match roots.classify(path) {
                Some(Area::Worktree) if lookup.is_ignored(path) => {}
                Some(area) => areas.push(area),
                None => {}
            }
        }
    }
    areas
}

/// Opens gix's ignore rules for the repo, once per burst of events.
struct IgnoreCheck {
    git_dir: PathBuf,
    workdir: PathBuf,
}

impl IgnoreCheck {
    fn new(git_dir: &Path, workdir: &Path) -> Self {
        Self {
            git_dir: git_dir.to_path_buf(),
            workdir: workdir.to_path_buf(),
        }
    }

    /// Loads `.gitignore` files, `info/exclude` and the user's excludes
    /// afresh: they may have just changed, and a stale cache would hide
    /// real changes. If anything fails the session ignores nothing, which
    /// errs toward an extra read, never a lost one.
    fn session(&self) -> IgnoreSession {
        let build = || -> Option<IgnoreSession> {
            let repo = gix::open(&self.git_dir).ok()?;
            let index = repo.index_or_empty().ok()?;
            let stack = repo
                .excludes(
                    &index,
                    None,
                    gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
                )
                .ok()?
                .detach();
            Some(IgnoreSession {
                stack: Some(stack),
                repo: Some(repo),
                workdir: self.workdir.clone(),
            })
        };
        build().unwrap_or(IgnoreSession {
            stack: None,
            repo: None,
            workdir: self.workdir.clone(),
        })
    }
}

struct IgnoreSession {
    stack: Option<gix::worktree::Stack>,
    repo: Option<gix::Repository>,
    workdir: PathBuf,
}

impl IgnoreSession {
    /// True when git would ignore `path` (absolute). gix's stack also
    /// treats a path inside an ignored folder as ignored.
    fn is_ignored(&mut self, path: &Path) -> bool {
        let (Some(stack), Some(repo)) = (self.stack.as_mut(), self.repo.as_ref()) else {
            return false;
        };
        let Ok(rel) = path.strip_prefix(&self.workdir) else {
            return false;
        };
        let mode = if path.is_dir() {
            gix::index::entry::Mode::DIR
        } else {
            gix::index::entry::Mode::FILE
        };
        stack
            .at_path(rel, Some(mode), &repo.objects)
            .is_ok_and(|platform| platform.is_excluded())
    }
}
