//! The actions the harness generates, and the strategies that generate
//! them. Every selection is a number resolved against the real repo when
//! the action runs (file `n` of the pool, hunk `n` modulo the hunk count),
//! so removing an action while shrinking never makes the rest invalid.

use proptest::prelude::*;

use crate::content::{Change, change};

/// Branch names the harness uses. `topic` and `topic/x` cannot both exist,
/// and `ü-branch` has a non-ASCII name.
pub const BRANCHES: [&str; 5] = ["main", "feat", "topic", "topic/x", "ü-branch"];

/// What the user picked in the diff view.
#[derive(Clone, Debug)]
pub enum Pick {
    File,
    Hunk(usize),
    /// A subset of the changed lines of one hunk: bit `i` of `mask` picks
    /// the `i`-th changed line (modulo 32).
    Lines {
        hunk: usize,
        mask: u32,
    },
}

/// A branch the action names: one of the branches that exist now (so the
/// action usually can work), or any name of the pool (so it often cannot).
#[derive(Clone, Debug)]
pub enum BranchSel {
    Existing(usize),
    Pool(usize),
}

/// Where a plain `git checkout` goes.
#[derive(Clone, Debug)]
pub enum Target {
    Branch(BranchSel),
    Detach,
}

/// One step of a sequence. The names line up with the Quint specs in
/// `specs/` (`stage`, `commit`, `outside_commit`, `hold_lock`, ...).
#[derive(Clone, Debug)]
pub enum Transition {
    // The user's editor changes files; the watcher tells the app.
    WriteFile {
        file: usize,
        change: Change,
    },
    RemoveFile {
        file: usize,
    },
    RenameFile {
        from: usize,
        to: usize,
    },
    // Through the app.
    Stage {
        file: usize,
        pick: Pick,
    },
    Unstage {
        file: usize,
        pick: Pick,
    },
    /// Stage the first hunk until none is left (laws 4 and 5).
    StageEveryHunk {
        file: usize,
    },
    /// Stage a selection of a file that has nothing staged, then unstage
    /// the file again (law 6).
    StageThenUnstage {
        file: usize,
        pick: Pick,
    },
    Commit {
        message: String,
        amend: bool,
    },
    CreateBranch {
        name: usize,
        switch: bool,
        start_back: bool,
    },
    SwitchBranch {
        name: BranchSel,
    },
    DeleteBranch {
        name: BranchSel,
        force: bool,
    },
    RenameBranch {
        from: BranchSel,
        to: usize,
    },
    StashSave {
        include_untracked: bool,
        message: String,
    },
    StashApply {
        entry: usize,
    },
    StashPop {
        entry: usize,
    },
    StashDrop {
        entry: usize,
    },
    Undo,
    Refresh,
    // Plain git or the filesystem behind the app's back.
    OutsideCommitAll {
        message: String,
    },
    OutsideCheckout {
        target: Target,
    },
    /// Another process holds `index.lock`; the app is asked for a write,
    /// must not touch `.git`, and runs it once the lock is gone (law 12).
    WhileLocked(Box<Transition>),
}

impl Transition {
    /// A transition the app can be asked for while `index.lock` is held.
    pub fn is_app_write(&self) -> bool {
        matches!(
            self,
            Transition::Stage { .. }
                | Transition::Unstage { .. }
                | Transition::Commit { .. }
                | Transition::CreateBranch { .. }
                | Transition::SwitchBranch { .. }
                | Transition::DeleteBranch { .. }
                | Transition::RenameBranch { .. }
                | Transition::StashSave { .. }
                | Transition::StashApply { .. }
                | Transition::StashPop { .. }
                | Transition::StashDrop { .. }
                | Transition::Undo
        )
    }
}

fn file() -> impl Strategy<Value = usize> {
    // Mostly the files that exist from the start.
    prop_oneof![5 => 0..3usize, 1 => 3..4usize]
}

fn pick() -> impl Strategy<Value = Pick> {
    prop_oneof![
        2 => Just(Pick::File),
        3 => (0..8usize).prop_map(Pick::Hunk),
        3 => (0..8usize, any::<u32>()).prop_map(|(hunk, mask)| Pick::Lines { hunk, mask }),
    ]
}

fn message() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => "[a-z]{1,8} [0-9]{1,2}",
        1 => Just("ünï 日本 😀".to_owned()),
    ]
}

fn stash_message() -> impl Strategy<Value = String> {
    prop_oneof![2 => Just(String::new()), 1 => "[a-z]{1,6} [0-9]"]
}

fn branch() -> impl Strategy<Value = usize> {
    0..BRANCHES.len()
}

fn branch_sel() -> impl Strategy<Value = BranchSel> {
    prop_oneof![
        3 => (0..8usize).prop_map(BranchSel::Existing),
        1 => branch().prop_map(BranchSel::Pool),
    ]
}

/// Everything the app can be asked to write.
fn app_write() -> impl Strategy<Value = Transition> {
    prop_oneof![
        10 => (file(), pick()).prop_map(|(file, pick)| Transition::Stage { file, pick }),
        3 => (file(), pick()).prop_map(|(file, pick)| Transition::Unstage { file, pick }),
        5 => (message(), prop_oneof![5 => Just(false), 1 => Just(true)])
            .prop_map(|(message, amend)| Transition::Commit { message, amend }),
        2 => (branch(), any::<bool>(), any::<bool>())
            .prop_map(|(name, switch, start_back)| Transition::CreateBranch {
                name,
                switch,
                start_back,
            }),
        3 => branch_sel().prop_map(|name| Transition::SwitchBranch { name }),
        2 => (branch_sel(), any::<bool>())
            .prop_map(|(name, force)| Transition::DeleteBranch { name, force }),
        1 => (branch_sel(), branch()).prop_map(|(from, to)| Transition::RenameBranch { from, to }),
        2 => (any::<bool>(), stash_message()).prop_map(|(include_untracked, message)| {
            Transition::StashSave {
                include_untracked,
                message,
            }
        }),
        1 => (0..4usize).prop_map(|entry| Transition::StashApply { entry }),
        1 => (0..4usize).prop_map(|entry| Transition::StashPop { entry }),
        1 => (0..4usize).prop_map(|entry| Transition::StashDrop { entry }),
        4 => Just(Transition::Undo),
    ]
}

pub fn transition() -> impl Strategy<Value = Transition> {
    prop_oneof![
        // Files change a lot, so diffs have something to stage.
        10 => (file(), change()).prop_map(|(file, change)| Transition::WriteFile { file, change }),
        1 => file().prop_map(|file| Transition::RemoveFile { file }),
        1 => (file(), file()).prop_map(|(from, to)| Transition::RenameFile { from, to }),
        26 => app_write(),
        3 => file().prop_map(|file| Transition::StageEveryHunk { file }),
        2 => (file(), pick()).prop_map(|(file, pick)| Transition::StageThenUnstage { file, pick }),
        1 => Just(Transition::Refresh),
        2 => message().prop_map(|message| Transition::OutsideCommitAll { message }),
        2 => prop_oneof![
                3 => branch_sel().prop_map(Target::Branch),
                1 => Just(Target::Detach),
            ]
            .prop_map(|target| Transition::OutsideCheckout { target }),
        3 => app_write().prop_map(|inner| Transition::WhileLocked(Box::new(inner))),
    ]
}
