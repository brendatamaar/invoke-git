use std::fs;
use std::path::PathBuf;
use std::process::Command as Process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use invoke_core::*;
use shell::precondition::RepoPaths;
use shell::undo::Snapshotted;
use shell::{Git, GitRunner, UndoRunner, read::read_event};

struct Repo {
    root: PathBuf,
    paths: RepoPaths,
    runner: GitRunner,
    undo: UndoRunner,
    state: RepoState,
}

impl Repo {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "invoke rebase {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let workdir = root.join("repo with spaces");
        let tools = root.join("installed tools");
        fs::create_dir_all(&workdir).unwrap();
        fs::create_dir_all(&tools).unwrap();
        let built = PathBuf::from(env!("CARGO_BIN_EXE_invoke-rebase-editor"));
        let editor = tools.join(built.file_name().unwrap());
        fs::copy(&built, &editor).unwrap();
        let paths = RepoPaths {
            git_dir: workdir.join(".git"),
            workdir,
        };
        let git = Git::detect("git").unwrap();
        let id = RepoId {
            root_commit: None,
            remote_url: None,
        };
        let repo = Self {
            runner: GitRunner::new(git.clone(), paths.clone())
                .with_env(Self::env(&root))
                .with_rebase_editor(editor),
            undo: UndoRunner::new(git, paths.clone()).with_env(Self::env(&root)),
            root,
            paths,
            state: RepoState::new(id),
        };
        fs::write(repo.root.join("empty-config"), "").unwrap();
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn env(root: &std::path::Path) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        [
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            (
                "GIT_CONFIG_GLOBAL",
                root.join("empty-config").into_os_string(),
            ),
            ("GIT_AUTHOR_NAME", "Tester".into()),
            ("GIT_AUTHOR_EMAIL", "tester@example.com".into()),
            ("GIT_COMMITTER_NAME", "Tester".into()),
            ("GIT_COMMITTER_EMAIL", "tester@example.com".into()),
            ("GIT_EDITOR", "false".into()),
            ("GIT_SEQUENCE_EDITOR", "false".into()),
            ("GIT_OPTIONAL_LOCKS", "0".into()),
        ]
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Process::new("git")
            .args(args)
            .current_dir(&self.paths.workdir)
            .envs(Self::env(&self.root))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    fn commit(&self, path: &str, content: &str, message: &str) -> Oid {
        self.write(path, content);
        self.git(&["add", path]);
        self.git(&["commit", "-qm", message]);
        Oid::from_hex(&self.git(&["rev-parse", "HEAD"])).unwrap()
    }

    fn write(&self, path: &str, content: &str) {
        fs::write(self.paths.workdir.join(path), content).unwrap();
    }

    fn deliver(&mut self, event: Event) {
        let (state, commands) = step(self.state.clone(), event);
        self.state = state;
        for command in commands {
            match command {
                Command::ReadState { read, .. } => self.deliver(read_event(&self.paths, read)),
                Command::SnapshotForUndo(point) => match self.undo.snapshot(&point) {
                    Snapshotted::Taken(event) => self.deliver(event),
                    Snapshotted::Failed(event) => {
                        self.deliver(event);
                        return;
                    }
                },
                Command::RunGit { op, args, expect } => {
                    let result = self.run(op, args, expect);
                    self.deliver(Event::OpFinished(op, result));
                }
                Command::ReplayUndo(replay) => self.deliver(self.undo.replay(&replay)),
                Command::PruneUndo(ids) => self.undo.prune(&ids),
                command => panic!("unexpected {command:?}"),
            }
        }
    }

    fn run(&self, op: OpId, args: GitArgs, expect: Expect) -> OpOutcome {
        let (tx, rx) = mpsc::channel();
        self.runner.run(op, args, expect, tx);
        loop {
            if let Event::OpFinished(_, outcome) = rx.recv_timeout(Duration::from_secs(30)).unwrap()
            {
                return outcome;
            }
        }
    }

    fn start(&mut self, base: Oid, steps: Vec<(Oid, RebaseAction)>) {
        self.deliver(Event::RefreshRequested);
        self.deliver(Event::RebasePlanEdited(
            RebasePlan::new(base, steps).unwrap(),
        ));
        self.deliver(Event::RebaseStartRequested);
    }

    fn succeeded(&self) {
        assert!(
            matches!(
                self.state.notice,
                Some(Notice::OpEnded {
                    outcome: OpOutcome::Succeeded,
                    ..
                })
            ),
            "{:?}",
            self.state.notice
        );
    }

    fn picture(&self) -> [String; 5] {
        [
            self.git(&["symbolic-ref", "HEAD"]),
            self.git(&["rev-parse", "HEAD"]),
            self.git(&[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
            ]),
            self.git(&["write-tree"]),
            self.git(&["status", "--porcelain"]),
        ]
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn reorder_squash_fixup_and_drop_only_rewrite_the_checked_out_branch() {
    let mut repo = Repo::new();
    let base = repo.commit("base", "base\n", "base");
    let a = repo.commit("a", "a\n", "pick a");
    repo.git(&["branch", "keep", a.as_hex()]);
    let b = repo.commit("b", "b\n", "pick b");
    let c = repo.commit("c", "c\n", "squash c");
    let d = repo.commit("d", "d\n", "fixup d");
    let e = repo.commit("e", "e\n", "drop e");
    repo.git(&["config", "rebase.updateRefs", "true"]);
    repo.git(&["config", "rebase.autosquash", "true"]);
    let before = repo.picture();
    repo.start(
        base.clone(),
        vec![
            (b, RebaseAction::Pick),
            (c, RebaseAction::Squash),
            (d, RebaseAction::Fixup),
            (a.clone(), RebaseAction::Pick),
            (e, RebaseAction::Drop),
        ],
    );
    repo.succeeded();
    assert_eq!(repo.state.rebase, RebaseState::Done);
    assert_eq!(repo.git(&["rev-parse", "keep"]), a.as_hex());
    let range = format!("{base}..HEAD");
    let messages = repo.git(&["log", "--reverse", "--format=%B", &range]);
    assert_eq!(messages, "pick b\n\nsquash c\n\npick a");
    assert!(!repo.paths.workdir.join("e").exists());
    repo.deliver(Event::UndoRequested);
    repo.succeeded();
    assert_eq!(repo.picture()[..4], before[..4]);
    assert!(
        !repo.paths.workdir.join("e").exists(),
        "undo preserves the worktree"
    );
}

#[test]
fn edit_can_amend_and_continue_or_abort_to_the_original_tree() {
    for abort in [false, true] {
        let mut repo = Repo::new();
        let base = repo.commit("base", "base\n", "base");
        let a = repo.commit("a", "a\n", "original a");
        let b = repo.commit("b", "b\n", "original b");
        let before = repo.picture();
        repo.start(base, vec![(a, RebaseAction::Edit), (b, RebaseAction::Pick)]);
        repo.succeeded();
        assert!(matches!(
            repo.state.fresh_disk().unwrap().phase,
            Phase::Rebasing {
                pause: RebasePause::Edit,
                ..
            }
        ));
        repo.deliver(Event::CommitRequested {
            amend: true,
            message: "amended a".into(),
        });
        repo.succeeded();
        repo.deliver(if abort {
            Event::AbortRequested
        } else {
            Event::ContinueRequested
        });
        repo.succeeded();
        if abort {
            assert_eq!(repo.picture(), before);
            assert_eq!(repo.state.rebase, RebaseState::Aborted);
            assert!(repo.state.undo.entries.is_empty());
        } else {
            assert_eq!(
                repo.git(&["log", "-2", "--format=%s"]),
                "original b\namended a"
            );
            repo.deliver(Event::UndoRequested);
            repo.succeeded();
            assert_eq!(repo.picture(), before);
        }
    }
}

#[test]
fn conflict_refuses_continue_until_resolved_and_supports_skip_and_abort() {
    for end in ["continue", "skip", "abort"] {
        let mut repo = Repo::new();
        let base = repo.commit("file", "base\n", "base");
        let a = repo.commit("file", "first\n", "first");
        let b = repo.commit("file", "second\n", "second");
        repo.write("untracked", "keep this\n");
        let before = repo.picture();
        repo.start(base, vec![(b, RebaseAction::Pick), (a, RebaseAction::Drop)]);
        assert!(matches!(
            repo.state.fresh_disk().unwrap().phase,
            Phase::Rebasing {
                pause: RebasePause::Conflict,
                ..
            }
        ));
        repo.deliver(Event::ContinueRequested);
        assert!(matches!(
            repo.state.notice,
            Some(Notice::Refused {
                reason: Refusal::Conflict(_),
                ..
            })
        ));
        let event = match end {
            "abort" => Event::AbortRequested,
            "skip" => Event::SkipRequested,
            _ => {
                repo.write("file", "resolved\n");
                repo.deliver(Event::RefreshRequested);
                repo.deliver(Event::ResolveFileRequested {
                    path: RepoPath("file".into()),
                    choice: ResolveChoice::Manual,
                });
                repo.succeeded();
                Event::ContinueRequested
            }
        };
        repo.deliver(event);
        repo.succeeded();
        assert_eq!(repo.state.fresh_disk().unwrap().phase, Phase::Clean);
        if end == "abort" {
            assert_eq!(repo.picture(), before);
            assert_eq!(
                fs::read(repo.paths.workdir.join("file")).unwrap(),
                b"second\n"
            );
        } else {
            repo.deliver(Event::UndoRequested);
            repo.succeeded();
            assert_eq!(repo.picture()[..4], before[..4]);
        }
        assert_eq!(
            fs::read(repo.paths.workdir.join("untracked")).unwrap(),
            b"keep this\n"
        );
    }
}

#[test]
fn shell_refuses_a_late_lock_or_a_moved_paused_branch() {
    let mut repo = Repo::new();
    let base = repo.commit("base", "base\n", "base");
    let a = repo.commit("a", "a\n", "first");
    repo.deliver(Event::RefreshRequested);
    let disk = repo.state.fresh_disk().unwrap().clone();
    let plan = RebasePlan::new(base.clone(), vec![(a.clone(), RebaseAction::Edit)]).unwrap();
    let lock = repo.paths.git_dir.join("index.lock");
    fs::write(&lock, "held").unwrap();
    let outcome = repo.run(
        OpId(42),
        GitArgs::InteractiveRebase(plan),
        Expect {
            head: disk.head,
            index: disk.index,
        },
    );
    assert!(matches!(
        outcome,
        OpOutcome::Failed(Failure {
            class: ErrorClass::PreconditionChanged,
            ..
        })
    ));
    assert_eq!(fs::read_to_string(&lock).unwrap(), "held");
    fs::remove_file(lock).unwrap();

    repo.start(base.clone(), vec![(a, RebaseAction::Edit)]);
    repo.succeeded();
    repo.git(&["update-ref", "refs/heads/main", base.as_hex()]);
    repo.deliver(Event::AbortRequested);
    assert!(matches!(
        repo.state.notice,
        Some(Notice::OpEnded {
            outcome: OpOutcome::Failed(Failure {
                class: ErrorClass::PreconditionChanged,
                ..
            }),
            ..
        })
    ));
    assert_eq!(repo.git(&["rev-parse", "main"]), base.as_hex());
    assert!(repo.paths.git_dir.join("rebase-merge").is_dir());
}
