//! Undo points kept from an earlier run are checked against the refs that
//! hold their saved trees (task 22, part 2). Generated repo, real git,
//! system and global config shut out.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use invoke_core::{
    HeadTarget, IndexEffect, IndexId, Irreversible, Oid, OpId, RepoPath, Reversal, Reversible,
    StashEntry, UndoEntry, UndoPoint,
};
use shell::precondition::RepoPaths;
use shell::undo::reconcile;

struct Repo {
    root: PathBuf,
    paths: RepoPaths,
    commit: String,
}

impl Repo {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "invoke-git-reconcile-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let workdir = root.join("repo");
        fs::create_dir_all(&workdir).unwrap();
        let mut repo = Self {
            paths: RepoPaths {
                git_dir: workdir.join(".git"),
                workdir,
            },
            root,
            commit: String::new(),
        };
        repo.git(&["init", "-q", "-b", "main"]);
        fs::write(repo.paths.workdir.join("a.txt"), "a\n").unwrap();
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "one"]);
        repo.commit = repo.git(&["rev-parse", "HEAD"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let global = self.root.join("gitconfig");
        fs::write(&global, "").unwrap();
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.paths.workdir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn keep(&self, name: &str) {
        self.git(&[
            "update-ref",
            &format!("refs/invoke-git/undo/{name}"),
            &self.commit,
        ]);
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn entry(op: u64, reverse: Reversal) -> UndoEntry {
    let reversible = match &reverse {
        Reversal::Never(why) => Reversible::No(*why),
        _ => Reversible::Yes,
    };
    UndoEntry {
        point: UndoPoint {
            op: OpId(op),
            label: format!("op {op}"),
            head: HeadTarget::Detached,
            refs: Vec::new(),
            index: IndexId::Absent,
            index_effect: IndexEffect::Untouched,
            reverse,
        },
        tree: Oid::from_hex(&"b".repeat(40)).unwrap(),
        after_refs: Vec::new(),
        after_index: IndexId::Absent,
        after_head: HeadTarget::Detached,
        reversible,
    }
}

fn stash() -> StashEntry {
    StashEntry {
        id: Oid::from_hex(&"c".repeat(40)).unwrap(),
        message: "On main: wip".into(),
    }
}

#[test]
fn a_point_is_kept_while_every_ref_it_needs_exists() {
    let repo = Repo::new();
    for name in [
        "1",
        "3",
        "4",
        "4-stash",
        "5",
        "5-discard",
        "5-discard-after",
        "6",
        "6-discard",
        "7",
        "7-stash",
        "8",
    ] {
        repo.keep(name);
    }
    let entries = vec![
        entry(1, Reversal::Refs),                // tree there: kept
        entry(2, Reversal::Refs),                // tree gone: dropped
        entry(3, Reversal::StashStore(stash())), // stash copy gone: dropped
        entry(4, Reversal::StashStore(stash())), // kept
        entry(
            5,
            Reversal::Discard {
                path: RepoPath("a.txt".into()),
            },
        ), // kept
        entry(
            6,
            Reversal::Discard {
                path: RepoPath("a.txt".into()),
            },
        ), // "after" copy gone: dropped
        entry(8, Reversal::Never(Irreversible::Push)), // a marker with its tree: kept
    ];
    let out = reconcile(&repo.paths, entries).unwrap();
    let kept: Vec<u64> = out.kept.iter().map(|e| e.point.op.0).collect();
    assert_eq!(kept, vec![1, 4, 5, 8]);
    // Refs of ops that are not kept: the dropped ones that still hold refs
    // (3 and 6) and an op that never had a point (7).
    let orphans: Vec<u64> = out.orphans.iter().map(|op| op.0).collect();
    assert_eq!(orphans, vec![3, 6, 7]);
}

#[test]
fn a_repo_with_no_saved_refs_keeps_nothing_and_has_nothing_to_prune() {
    let repo = Repo::new();
    let out = reconcile(&repo.paths, vec![entry(1, Reversal::Refs)]).unwrap();
    assert!(out.kept.is_empty() && out.orphans.is_empty());
    let none = reconcile(&repo.paths, Vec::new()).unwrap();
    assert!(none.kept.is_empty() && none.orphans.is_empty());
}
