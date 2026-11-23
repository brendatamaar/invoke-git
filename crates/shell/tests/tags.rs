//! P3: tags end to end: the real core driving the real shell on generated
//! temp repos with a local bare remote, real git as the oracle. System and
//! global git config are shut out. Law 8 (undo of local create and delete),
//! law 9 (only the targeted tag moves; remote tag writes are irreversible).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use invoke_core::{
    Action, BranchStart, ChangeSet, Command as Cmd, ErrorClass, Event, Notice, OpId, OpOutcome,
    Refusal, RepoId, RepoState, TagName, TagRefusal, UndoMoved, open, step,
};
use shell::precondition::RepoPaths;
use shell::undo::Snapshotted;
use shell::{Git, GitRunner, UndoRunner, read::read_event};

struct Fx {
    root: PathBuf,
    paths: RepoPaths,
    env: Vec<(OsString, OsString)>,
}

impl Fx {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let root = std::env::temp_dir().join(format!(
            "invoke-git-tags-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let workdir = root.join("repo");
        fs::create_dir_all(&workdir).unwrap();
        let global = root.join("gitconfig");
        fs::write(&global, "").unwrap();
        let env = [
            ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
            ("GIT_CONFIG_GLOBAL", global.into_os_string()),
            ("GIT_AUTHOR_NAME", "Test".into()),
            ("GIT_AUTHOR_EMAIL", "test@example.com".into()),
            ("GIT_COMMITTER_NAME", "Test".into()),
            ("GIT_COMMITTER_EMAIL", "test@example.com".into()),
        ]
        .into_iter()
        .map(|(key, value)| (OsString::from(key), value))
        .collect();
        let fx = Self {
            paths: RepoPaths {
                git_dir: workdir.join(".git"),
                workdir,
            },
            root,
            env,
        };
        fx.git(
            &fx.root,
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        let w = fx.paths.workdir.clone();
        fx.git(&w, &["init", "-q", "-b", "main"]);
        fx.git(
            &w,
            &["remote", "add", "origin", fx.origin().to_str().unwrap()],
        );
        fx.commit("a.txt", "one\n", "c1");
        fx.commit("a.txt", "two\n", "c2");
        fx
    }

    fn origin(&self) -> PathBuf {
        self.root.join("origin.git")
    }

    fn try_git(&self, dir: &Path, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        self.try_git(dir, args)
            .unwrap_or_else(|| panic!("git {args:?} failed"))
    }

    fn local(&self, args: &[&str]) -> String {
        self.git(&self.paths.workdir, args)
    }

    fn commit(&self, path: &str, text: &str, message: &str) {
        fs::write(self.paths.workdir.join(path), text).unwrap();
        self.local(&["add", path]);
        self.local(&["commit", "-q", "-m", message]);
    }

    /// Every ref but our bookkeeping, in a repo.
    fn refs(&self, dir: &Path) -> BTreeMap<String, String> {
        self.git(dir, &["for-each-ref", "--format=%(refname) %(objectname)"])
            .lines()
            .filter(|line| !line.starts_with("refs/invoke-git/"))
            .map(|line| {
                let (name, oid) = line.split_once(' ').unwrap();
                (name.to_owned(), oid.to_owned())
            })
            .collect()
    }

    fn local_refs(&self) -> BTreeMap<String, String> {
        self.refs(&self.paths.workdir)
    }

    fn remote_tags(&self) -> BTreeSet<String> {
        self.refs(&self.origin())
            .into_keys()
            .filter(|name| name.starts_with("refs/tags/"))
            .collect()
    }
}

fn moved(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> BTreeSet<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|name| before.get(*name) != after.get(*name))
        .cloned()
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn tag(n: &str) -> TagName {
    TagName(n.into())
}

struct Driver<'a> {
    fx: &'a Fx,
    state: RepoState,
    runner: GitRunner,
    undo: UndoRunner,
    skip: BTreeSet<OpId>,
}

impl<'a> Driver<'a> {
    fn open(fx: &'a Fx) -> Self {
        let git = || Git::detect("git").unwrap();
        let (state, commands) = open(RepoId {
            root_commit: None,
            remote_url: None,
        });
        let mut driver = Self {
            fx,
            state,
            runner: GitRunner::new(git(), fx.paths.clone()).with_env(fx.env.clone()),
            undo: UndoRunner::new(git(), fx.paths.clone()).with_env(fx.env.clone()),
            skip: BTreeSet::new(),
        };
        for command in commands {
            driver.exec(command);
        }
        driver
    }

    fn deliver(&mut self, event: Event) {
        let (state, commands) = step(self.state.clone(), event);
        self.state = state;
        for command in commands {
            self.exec(command);
        }
    }

    fn exec(&mut self, command: Cmd) {
        match command {
            Cmd::ReadState { read, .. } => self.deliver(read_event(&self.fx.paths, read)),
            Cmd::SnapshotForUndo(point) => match self.undo.snapshot(&point) {
                Snapshotted::Taken(event) => self.deliver(event),
                Snapshotted::Failed(event) => {
                    self.skip.insert(point.op);
                    self.deliver(event);
                }
            },
            Cmd::RunGit { op, args, expect } => {
                if self.skip.remove(&op) {
                    return;
                }
                let (sender, events) = mpsc::channel();
                self.runner.run(op, args, expect, sender);
                loop {
                    let event = events
                        .recv_timeout(Duration::from_secs(30))
                        .expect("no OpFinished in 30 s");
                    let done = matches!(event, Event::OpFinished(..));
                    self.deliver(event);
                    if done {
                        break;
                    }
                }
            }
            Cmd::ReplayUndo(replay) => self.deliver(self.undo.replay(&replay)),
            Cmd::PruneUndo(ops) => self.undo.prune(&ops),
            other => panic!("unexpected command {other:?}"),
        }
    }

    fn outside_change(&mut self) {
        self.deliver(Event::DiskChanged(ChangeSet(
            invoke_core::event::ALL_AREAS.into_iter().collect(),
        )));
    }

    fn create(&mut self, name: &str, message: Option<&str>) {
        self.deliver(Event::TagCreateRequested {
            name: name.into(),
            at: BranchStart::Head,
            message: message.map(str::to_owned),
        });
    }

    fn delete(&mut self, name: &str) {
        self.deliver(Event::TagDeleteRequested(tag(name)));
    }

    fn push(&mut self, name: &str) {
        self.deliver(Event::TagPushRequested {
            remote: "origin".into(),
            name: tag(name),
        });
    }

    fn delete_remote(&mut self, name: &str) {
        self.deliver(Event::RemoteTagDeleteRequested {
            remote: "origin".into(),
            name: tag(name),
        });
    }

    fn undo(&mut self) {
        self.deliver(Event::UndoRequested);
    }

    fn outcome(&self) -> Option<OpOutcome> {
        match &self.state.notice {
            Some(Notice::OpEnded { outcome, .. }) => Some(outcome.clone()),
            _ => None,
        }
    }

    fn succeeded(&self) -> bool {
        self.outcome() == Some(OpOutcome::Succeeded)
    }

    fn refusal(&self) -> Option<(Action, Refusal)> {
        match &self.state.notice {
            Some(Notice::Refused { action, reason }) => Some((*action, reason.clone())),
            _ => None,
        }
    }

    fn tag_names(&self) -> Vec<String> {
        self.state
            .fresh_disk()
            .map(|disk| disk.refs.tags().into_iter().map(|t| t.name.0).collect())
            .unwrap_or_default()
    }
}

#[test]
fn create_lightweight_moves_only_that_tag_and_undo_removes_it() {
    let fx = Fx::new();
    let before = fx.local_refs();
    let mut driver = Driver::open(&fx);
    driver.create("v1", None);
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    let after = fx.local_refs();
    assert_eq!(moved(&before, &after), set(&["refs/tags/v1"]));
    // Lightweight: the ref names the commit itself.
    assert_eq!(after["refs/tags/v1"], fx.local(&["rev-parse", "HEAD"]));
    assert_eq!(fx.local(&["cat-file", "-t", "v1"]), "commit");
    // The new tag shows in the state after the refresh.
    assert_eq!(driver.tag_names(), ["v1"]);

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(fx.local_refs(), before);
    assert!(driver.tag_names().is_empty());
}

#[test]
fn create_annotated_makes_a_tag_object_that_peels_to_the_commit() {
    let fx = Fx::new();
    let mut driver = Driver::open(&fx);
    driver.create("v2", Some("Release 2\n\nNotes"));
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(fx.local(&["cat-file", "-t", "v2"]), "tag");
    assert_eq!(
        fx.local(&["tag", "-l", "--format=%(contents:subject)", "v2"]),
        "Release 2"
    );
    let head = fx.local(&["rev-parse", "HEAD"]);
    let disk = driver.state.fresh_disk().unwrap();
    let info = &disk.refs.tags()[0];
    assert!(info.annotated);
    assert_eq!(info.target.as_hex(), head);
    assert_ne!(info.tip.as_hex(), head);

    driver.undo();
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert!(
        fx.try_git(
            &fx.paths.workdir,
            &["rev-parse", "-q", "--verify", "refs/tags/v2"]
        )
        .is_none()
    );
}

#[test]
fn delete_local_and_undo_restores_the_exact_ref_even_annotated() {
    let fx = Fx::new();
    fx.local(&["tag", "light"]);
    fx.local(&["tag", "-a", "-m", "m", "noted"]);
    let before = fx.local_refs();
    let mut driver = Driver::open(&fx);
    for name in ["light", "noted"] {
        driver.delete(name);
        assert!(driver.succeeded(), "{:?}", driver.state.notice);
        assert_eq!(
            moved(&before, &fx.local_refs()),
            set(&[format!("refs/tags/{name}").as_str()])
        );
        driver.undo();
        assert!(driver.succeeded(), "{:?}", driver.state.notice);
        assert_eq!(fx.local_refs(), before, "undo of deleting {name}");
    }
}

#[test]
fn push_then_delete_remote_changes_only_the_remote_and_cannot_be_undone() {
    let fx = Fx::new();
    fx.local(&["tag", "v1"]);
    fx.local(&["tag", "v9"]);
    let before = fx.local_refs();
    let mut driver = Driver::open(&fx);

    driver.push("v1");
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert_eq!(fx.remote_tags(), set(&["refs/tags/v1"]));
    assert_eq!(fx.local_refs(), before, "a tag push moves no local ref");

    // Law 9: the push is irreversible.
    driver.undo();
    assert!(matches!(
        driver.refusal(),
        Some((Action::Undo, Refusal::NotUndoable(_)))
    ));
    assert_eq!(fx.remote_tags(), set(&["refs/tags/v1"]));

    driver.delete_remote("v1");
    assert!(driver.succeeded(), "{:?}", driver.state.notice);
    assert!(fx.remote_tags().is_empty());
    assert_eq!(fx.local_refs(), before, "the local tag stays");
}

#[test]
fn refusals_name_the_reason_and_write_nothing() {
    let fx = Fx::new();
    fx.local(&["tag", "v1"]);
    let before = fx.local_refs();
    let mut driver = Driver::open(&fx);

    driver.create("v1", None);
    assert_eq!(
        driver.refusal(),
        Some((
            Action::CreateTag,
            Refusal::Tag(TagRefusal::AlreadyExists(tag("v1")))
        ))
    );
    driver.create("v1/x", None);
    assert_eq!(
        driver.refusal(),
        Some((
            Action::CreateTag,
            Refusal::Tag(TagRefusal::NameConflict(tag("v1")))
        ))
    );
    driver.create("bad name", None);
    assert!(matches!(
        driver.refusal(),
        Some((Action::CreateTag, Refusal::Tag(TagRefusal::InvalidName(_))))
    ));
    driver.delete("nope");
    assert_eq!(
        driver.refusal(),
        Some((
            Action::DeleteTag,
            Refusal::Tag(TagRefusal::NoSuchTag(tag("nope")))
        ))
    );
    driver.push("nope");
    assert_eq!(
        driver.refusal(),
        Some((
            Action::PushTag,
            Refusal::Tag(TagRefusal::NoSuchTag(tag("nope")))
        ))
    );
    assert_eq!(fx.local_refs(), before);
}

/// A pushed tag the remote already holds at another value is refused by the
/// remote (no force): an expected outcome, the remote keeps its value.
#[test]
fn pushing_over_a_different_remote_tag_is_refused_by_the_remote() {
    let fx = Fx::new();
    fx.local(&["tag", "v1"]);
    let mut driver = Driver::open(&fx);
    driver.push("v1");
    assert!(driver.succeeded());
    let remote_before = fx.refs(&fx.origin());

    fx.local(&["tag", "-d", "v1"]);
    fx.commit("a.txt", "three\n", "c3");
    fx.local(&["tag", "v1"]);
    driver.outside_change();
    driver.push("v1");
    match driver.outcome() {
        Some(OpOutcome::Failed(failure)) => assert_eq!(failure.class, ErrorClass::Expected),
        other => panic!("expected a refused push, got {other:?}"),
    }
    assert_eq!(fx.refs(&fx.origin()), remote_before);
}

/// Rule 5: a tag that changed on disk since the snapshot stops the delete
/// and its undo is refused when something else moved it.
#[test]
fn delete_is_refused_when_the_tag_moved_since_the_snapshot() {
    let fx = Fx::new();
    fx.local(&["tag", "v1"]);
    let mut driver = Driver::open(&fx);
    // Move the tag behind the core's back, with no watcher event yet.
    fx.local(&["tag", "-f", "v1", "HEAD~1"]);
    let before = fx.local_refs();
    driver.delete("v1");
    match driver.outcome() {
        Some(OpOutcome::Failed(failure)) => {
            assert_eq!(failure.class, ErrorClass::PreconditionChanged);
        }
        other => panic!("expected a stale-state refusal, got {other:?}"),
    }
    assert_eq!(fx.local_refs(), before);
}

#[test]
fn undo_of_create_is_refused_when_the_tag_moved_since() {
    let fx = Fx::new();
    let mut driver = Driver::open(&fx);
    driver.create("v1", None);
    assert!(driver.succeeded());
    fx.local(&["tag", "-f", "v1", "HEAD~1"]);
    driver.outside_change();
    driver.undo();
    assert_eq!(
        driver.refusal(),
        Some((
            Action::Undo,
            Refusal::Changed(UndoMoved::Ref(tag("v1").to_ref()))
        ))
    );
}
