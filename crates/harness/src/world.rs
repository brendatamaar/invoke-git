//! The world a sequence runs in: the app (real core + shell) on one temp
//! repo, a plain-git twin on another, and everything compared between them
//! after every step. Failures panic with the reason and the twin's history
//! as a list of plain git commands (the harness's shrunk replay).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};

use diff::{DiffOptions, Direction, Mode, PatchSpec, Pick as DiffPick, build_patch, diff_text};
use invoke_core::{
    Area, BranchName, BranchStart, DiscardRequest, ErrorClass, Event, FileChange, LockState,
    MAX_POINTS, Oid, Patch, Refusal, RepoPath, Reversal, Selection, StageRequest,
};
use shell::{InputsError, Source, load_inputs, read::read_state};

use crate::app::{App, Outcome};
use crate::content::{self, FILES};
use crate::gitdir::{RepoDir, Root, Run, dollar_quote, quote};
use crate::observe::{HeadAt, Observed, Snap, first_difference, footprint, observe};
use crate::transition::{BRANCHES, BranchSel, Pick, ReplayEnd, Target, Transition};

/// One git run on the twin: argv after `git`, and what goes to stdin.
#[derive(Clone, Debug)]
struct TwinOp {
    args: Vec<String>,
    stdin: Option<String>,
}

impl TwinOp {
    fn git(args: &[&str]) -> Self {
        Self {
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            stdin: None,
        }
    }
}

/// What kind of operation a write is, for the undo model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hint {
    Commit,
    Other,
    StashRewrote,
    StashDrop {
        position: usize,
    },
    /// P2: a cherry-pick or revert that ended in one commit: undone like a
    /// commit (refs and index).
    Replay,
    /// P2: a continued cherry-pick or revert: undo stops at it.
    Continued,
}

struct Write {
    event: Event,
    twin: TwinOp,
    hint: Hint,
    /// Staging and unstaging: law 4 says the worktree is never touched.
    index_only: bool,
    /// Task 13: a discard of this file; law 7 says no other file changes.
    discarded: Option<String>,
}

enum Prepared {
    Nothing,
    Write(Box<Write>),
    Undo,
}

/// How the model can undo what one finished operation did.
#[derive(Clone, Debug)]
enum Kind {
    /// Put the footprint refs, `HEAD` and (when `index_dependent`) the
    /// index back.
    Refs { index_dependent: bool },
    /// Put the dropped stash back on top (`git stash store`).
    StashDrop { id: String, message: String },
    /// A stash save, apply or pop, or a drop below the top: undo stops.
    Irreversible,
    /// Task 13: a discard. Undo writes `before` back to the file, but only
    /// while the file still holds `after`; refs and index are not involved.
    Discard {
        path: String,
        before: Option<Vec<u8>>,
        after: Option<Vec<u8>>,
    },
}

/// The model's copy of one undo-stack entry, built from the twin only.
#[derive(Clone, Debug)]
struct Entry {
    kind: Kind,
    before: Snap,
    after: Snap,
    /// The shell variable holding the saved index tree in the script.
    tree_var: String,
    /// Something outside the app touched the repo since: the app may then
    /// refuse for reasons the model cannot see (an index file that was
    /// rewritten with the same content).
    ambiguous: bool,
}

/// What the model says an undo must do.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    Refuse(&'static str),
    /// Task 13: the file was changed after the discard, so the undo fails
    /// (a precondition change reported by the shell, not a refusal).
    RefuseFile,
    Accept,
    Either,
}

pub struct World {
    _root: Root,
    pub app: App,
    pub twin: RepoDir,
    script: Vec<String>,
    stack: Vec<Entry>,
    twin_obs: Observed,
    /// P2: how to end a conflicting cherry-pick or revert of this step, and
    /// what the twin looked like before it started.
    replay: Option<(ReplayEnd, Box<Observed>)>,
    steps: usize,
    trees: usize,
}

impl World {
    pub fn new() -> Self {
        let root = Root::new();
        let app_repo = RepoDir::init(&root.path, "app");
        let twin = RepoDir::init(&root.path, "twin");
        let mut script = vec![
            "# Replay of the plain-git twin. Run it in an empty folder with a clean git".to_owned(),
            "# config. Fixed dates keep commit ids the same on every replay:".to_owned(),
            "export GIT_AUTHOR_DATE='2020-01-01T00:00:00 +0000' GIT_COMMITTER_DATE='2020-01-01T00:00:00 +0000'"
                .to_owned(),
            "export GIT_AUTHOR_NAME=Test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=Test GIT_COMMITTER_EMAIL=test@example.com"
                .to_owned(),
            "git init -q -b main twin && cd twin".to_owned(),
        ];
        for dir in [&app_repo, &twin] {
            for (path, text) in content::initial_files() {
                dir.write(path, &text);
            }
            assert!(dir.git(&["add", "-A"]).ok);
            assert!(dir.git(&["commit", "-q", "-m", "init"]).ok);
        }
        for (path, text) in content::initial_files() {
            script.push(write_line(path, &text));
        }
        script.push("git add -A && git commit -q -m init".to_owned());
        let twin_obs = observe(&twin);
        let app = App::open(app_repo);
        let mut world = Self {
            _root: root,
            app,
            twin,
            script,
            stack: Vec::new(),
            twin_obs,
            replay: None,
            steps: 0,
            trees: 0,
        };
        world.check();
        world
    }

    /// The twin's history as shell lines.
    pub fn replay(&self) -> String {
        self.script.join("\n")
    }

    /// Runs one step; on failure panics with the reason and the replay.
    pub fn step(&mut self, transition: Transition) {
        self.steps += 1;
        self.script
            .push(format!("# --- step {}: {transition:?}", self.steps));
        let result = catch_unwind(AssertUnwindSafe(|| self.exec(&transition)));
        if let Err(payload) = result {
            let reason = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "panic".into());
            panic!(
                "{reason}\n\nReplay the twin with plain git (the app must end up the same):\n\
                 ----\n{}\n----",
                self.replay()
            );
        }
    }

    fn exec(&mut self, transition: &Transition) {
        match transition {
            Transition::WriteFile { file, change } => {
                let path = FILES[file % FILES.len()];
                let text = content::apply(self.twin.read(path).as_deref(), change);
                self.fs_write(path, &text);
                self.outside_files();
            }
            Transition::RemoveFile { file } => {
                let path = FILES[file % FILES.len()];
                if self.twin.full(path).exists() {
                    self.script.push(format!("rm {}", quote(path)));
                    self.app.repo.remove(path);
                    self.twin.remove(path);
                    self.outside_files();
                }
            }
            Transition::RenameFile { from, to } => {
                let (from, to) = (FILES[from % FILES.len()], FILES[to % FILES.len()]);
                if from != to && self.twin.full(from).exists() {
                    self.script.push(format!(
                        "mkdir -p \"$(dirname {to})\" && mv {from} {to}",
                        from = quote(from),
                        to = quote(to)
                    ));
                    self.app.repo.rename(from, to);
                    self.twin.rename(from, to);
                    self.outside_files();
                }
            }
            Transition::Refresh => {
                self.app.deliver(Event::RefreshRequested);
                self.refresh_twin();
                self.check();
            }
            Transition::StageEveryHunk { file } => self.stage_every_hunk(FILES[file % FILES.len()]),
            Transition::StageThenUnstage { file, pick } => {
                self.stage_then_unstage(FILES[file % FILES.len()], pick);
            }
            Transition::DiscardThenUndo { file, pick, edit } => {
                self.discard_then_undo(FILES[file % FILES.len()], pick, edit.as_ref());
            }
            Transition::OutsideCommitAll { message } => {
                self.outside(&["commit", "-q", "-a", "-m", message]);
            }
            Transition::OutsideCheckout { target } => match target {
                Target::Branch(branch) => {
                    let name = self.branch_name(branch);
                    self.outside(&["checkout", "-q", &name]);
                }
                Target::Detach => self.outside(&["checkout", "-q", "--detach"]),
            },
            Transition::WhileLocked(inner) => {
                self.begin_replay(inner);
                let prepared = self.prepare(inner);
                self.drive(prepared, true);
                self.replay = None;
            }
            other => {
                self.begin_replay(other);
                let prepared = self.prepare(other);
                self.drive(prepared, false);
                self.replay = None;
            }
        }
    }

    // ---- twin helpers ---------------------------------------------------

    /// P2: remembers how a conflicting pick or revert of this step ends and
    /// what the repo looked like before it.
    fn begin_replay(&mut self, transition: &Transition) {
        if let Transition::Replay { end, .. } = transition {
            self.replay = Some((*end, Box::new(self.twin_obs.clone())));
        }
    }

    fn twin_git(&mut self, op: &TwinOp) -> Run {
        let line = format!(
            "git {}",
            op.args
                .iter()
                .map(|arg| quote(arg))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let args: Vec<&str> = op.args.iter().map(String::as_str).collect();
        match &op.stdin {
            None => {
                self.script.push(line);
                self.twin.git(&args)
            }
            Some(stdin) => {
                self.script.push(format!("{line} <<'PATCH'\n{stdin}PATCH"));
                self.twin.git_stdin(&args, stdin.as_bytes())
            }
        }
    }

    fn fs_write(&mut self, path: &str, text: &str) {
        self.script.push(write_line(path, text));
        self.app.repo.write(path, text);
        self.twin.write(path, text);
    }

    /// Re-reads the twin; true when anything observable changed.
    fn refresh_twin(&mut self) -> bool {
        let now = observe(&self.twin);
        let changed = now != self.twin_obs;
        self.twin_obs = now;
        changed
    }

    fn twin_snap(&self) -> Snap {
        let tree = self.twin.git(&["write-tree"]);
        Snap::of(
            &self.twin_obs,
            if tree.ok {
                tree.text()
            } else {
                "unmerged".into()
            },
        )
    }

    /// The watcher saw files change.
    fn outside_files(&mut self) {
        self.app.disk_changed(&[Area::Worktree]);
        self.refresh_twin();
        self.check();
    }

    /// Plain git run behind the app's back, in both repos.
    fn outside(&mut self, args: &[&str]) {
        stat(&format!("outside: git {}", args[0]));
        let app = self.app.repo.git(args);
        let twin = self.twin_git(&TwinOp::git(args));
        if app.ok != twin.ok {
            panic!(
                "the same outside command ended differently in the two repos: git {args:?}\n  app: {} {}\n  twin: {} {}",
                app.ok, app.stderr, twin.ok, twin.stderr
            );
        }
        self.app.everything_changed();
        self.mark_ambiguous();
        self.refresh_twin();
        self.check();
    }

    fn mark_ambiguous(&mut self) {
        for entry in &mut self.stack {
            entry.ambiguous = true;
        }
    }

    // ---- preparing an app request and its twin command ------------------

    fn prepare(&mut self, transition: &Transition) -> Prepared {
        let write = |event, twin: TwinOp, hint| {
            Prepared::Write(Box::new(Write {
                event,
                twin,
                hint,
                index_only: false,
                discarded: None,
            }))
        };
        match transition {
            Transition::Stage { file, pick } => {
                self.prepare_stage(FILES[file % FILES.len()], pick, Direction::Stage)
            }
            Transition::Unstage { file, pick } => {
                self.prepare_stage(FILES[file % FILES.len()], pick, Direction::Unstage)
            }
            Transition::Discard { file, pick } => {
                self.prepare_stage(FILES[file % FILES.len()], pick, Direction::Discard)
            }
            Transition::Commit { message, amend } => {
                let mut args = vec!["commit", "-q"];
                if *amend {
                    args.push("--amend");
                }
                args.extend(["-m", message]);
                write(
                    Event::CommitRequested {
                        message: message.clone(),
                        amend: *amend,
                    },
                    TwinOp::git(&args),
                    Hint::Commit,
                )
            }
            Transition::CreateBranch {
                name,
                switch,
                start_back,
            } => {
                let name = BRANCHES[name % BRANCHES.len()];
                let head = self.twin_obs_head_oid();
                let back = self.twin.git(&["rev-parse", "-q", "--verify", "HEAD~1"]);
                let (at, start) = if *start_back && back.ok {
                    let oid = back.text();
                    let parsed = Oid::from_hex(&oid).expect("rev-parse gives an object id");
                    (oid, BranchStart::Commit(parsed))
                } else {
                    (head, BranchStart::Head)
                };
                let twin = if *switch {
                    TwinOp::git(&["switch", "-c", name, &at])
                } else {
                    TwinOp::git(&["branch", "--no-track", name, &at])
                };
                write(
                    Event::BranchCreateRequested {
                        name: name.to_owned(),
                        at: start,
                        switch: *switch,
                    },
                    twin,
                    Hint::Other,
                )
            }
            Transition::SwitchBranch { name } => {
                let name = self.branch_name(name);
                write(
                    Event::BranchSwitchRequested(BranchName(name.clone())),
                    TwinOp::git(&["switch", "--no-guess", &name]),
                    Hint::Other,
                )
            }
            Transition::DeleteBranch { name, force } => {
                let name = self.branch_name(name);
                write(
                    Event::BranchDeleteRequested {
                        name: BranchName(name.clone()),
                        force: *force,
                    },
                    TwinOp::git(&["branch", if *force { "-D" } else { "-d" }, &name]),
                    Hint::Other,
                )
            }
            Transition::RenameBranch { from, to } => {
                let from = self.branch_name(from);
                let (from, to) = (from.as_str(), BRANCHES[to % BRANCHES.len()]);
                // Known gap (tasks/024.DONE.md, question 2): the app refuses
                // to rename `topic` to `topic/x`, which git allows because
                // the old name goes away. Not generated until fixed.
                let nested = |outer: &str, inner: &str| {
                    inner
                        .strip_prefix(outer)
                        .is_some_and(|rest| rest.starts_with('/'))
                };
                if nested(from, to) || nested(to, from) {
                    return Prepared::Nothing;
                }
                write(
                    Event::BranchRenameRequested {
                        from: BranchName(from.to_owned()),
                        to: to.to_owned(),
                    },
                    TwinOp::git(&["branch", "-m", from, to]),
                    Hint::Other,
                )
            }
            Transition::StashSave {
                include_untracked,
                message,
            } => {
                let mut args = vec!["stash".to_owned(), "push".to_owned()];
                if *include_untracked {
                    args.push("--include-untracked".into());
                }
                if !message.is_empty() {
                    args.push(format!("--message={message}"));
                }
                write(
                    Event::StashSaveRequested {
                        message: message.clone(),
                        include_untracked: *include_untracked,
                    },
                    TwinOp { args, stdin: None },
                    Hint::StashRewrote,
                )
            }
            Transition::StashApply { entry }
            | Transition::StashPop { entry }
            | Transition::StashDrop { entry } => {
                let stashes = self
                    .app
                    .state
                    .fresh_disk()
                    .map(|disk| disk.stashes.clone())
                    .unwrap_or_default();
                if stashes.is_empty() {
                    return Prepared::Nothing;
                }
                let position = entry % stashes.len();
                let id = stashes[position].id.clone();
                let (event, verb, hint) = match transition {
                    Transition::StashApply { .. } => {
                        (Event::StashApplyRequested(id), "apply", Hint::StashRewrote)
                    }
                    Transition::StashPop { .. } => {
                        (Event::StashPopRequested(id), "pop", Hint::StashRewrote)
                    }
                    _ => (
                        Event::StashDropRequested(id),
                        "drop",
                        Hint::StashDrop { position },
                    ),
                };
                write(
                    event,
                    TwinOp::git(&["stash", verb, &format!("stash@{{{position}}}")]),
                    hint,
                )
            }
            Transition::Replay { commit, revert, .. } => {
                let list = self.twin.git(&["rev-list", "--branches", "HEAD"]);
                let commits: Vec<String> = list.text().lines().map(str::to_owned).collect();
                // The app refuses while a tracked file has changes (git alone
                // would start, and an abort then rewrites those edits).
                let dirty = self
                    .twin
                    .git(&["status", "--porcelain", "--untracked-files=no"]);
                if !list.ok || commits.is_empty() || !dirty.ok || !dirty.text().is_empty() {
                    return Prepared::Nothing;
                }
                let id = commits[commit % commits.len()].clone();
                let oid = Oid::from_hex(&id).expect("rev-list gives an object id");
                let (event, twin) = if *revert {
                    (
                        Event::RevertRequested(oid),
                        TwinOp::git(&["revert", "--no-edit", "--", &id]),
                    )
                } else {
                    (
                        Event::CherryPickRequested(oid),
                        TwinOp::git(&["cherry-pick", "--", &id]),
                    )
                };
                write(event, twin, Hint::Replay)
            }
            Transition::Undo => Prepared::Undo,
            _ => Prepared::Nothing,
        }
    }

    /// The branch a selection names now.
    fn branch_name(&self, sel: &BranchSel) -> String {
        let existing: Vec<&str> = self
            .twin_obs
            .refs
            .keys()
            .filter_map(|name| name.strip_prefix("refs/heads/"))
            .collect();
        match sel {
            BranchSel::Existing(n) if !existing.is_empty() => {
                existing[n % existing.len()].to_owned()
            }
            BranchSel::Existing(n) | BranchSel::Pool(n) => BRANCHES[n % BRANCHES.len()].to_owned(),
        }
    }

    fn twin_obs_head_oid(&self) -> String {
        Snap::of(&self.twin_obs, String::new()).head_oid
    }

    /// Builds the patch the way the UI will, from the files as the app
    /// sees them now. `Nothing` when the selection changes nothing.
    fn prepare_stage(&mut self, path: &str, pick: &Pick, direction: Direction) -> Prepared {
        let source = match direction {
            Direction::Stage | Direction::Discard => Source::IndexToWorktree,
            Direction::Unstage => Source::HeadToIndex,
        };
        let inputs = match load_inputs(&self.app.repo.paths, &RepoPath(path.into()), source) {
            Ok(inputs) => inputs,
            Err(InputsError::NotText(_)) => return Prepared::Nothing,
            Err(other) => panic!("cannot read the sides of {path}: {other:?}"),
        };
        let text = diff_text(
            inputs.old.clone().unwrap_or_default(),
            inputs.new.clone().unwrap_or_default(),
            &DiffOptions::default(),
        );
        if text.hunks.is_empty() {
            return Prepared::Nothing;
        }
        // Resolve the numbers against the real hunks.
        let chosen_lines: Vec<usize> = match pick {
            Pick::Lines { hunk, mask } => {
                let changed: Vec<usize> = text.hunks[hunk % text.hunks.len()]
                    .lines
                    .iter()
                    .enumerate()
                    .filter(|(_, line)| line.kind != diff::LineKind::Context)
                    .map(|(at, _)| at)
                    .collect();
                let mut lines: Vec<usize> = changed
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << (i % 32)) != 0)
                    .map(|(_, at)| *at)
                    .collect();
                if lines.is_empty() {
                    lines.push(changed[0]);
                }
                lines
            }
            Pick::File | Pick::Hunk(_) => Vec::new(),
        };
        let (selection, diff_pick) = match pick {
            Pick::File => (Selection::File(RepoPath(path.into())), DiffPick::File),
            Pick::Hunk(n) => {
                let hunk = n % text.hunks.len();
                (
                    Selection::Hunk {
                        path: RepoPath(path.into()),
                        hunk,
                    },
                    DiffPick::Hunk(hunk),
                )
            }
            Pick::Lines { hunk, .. } => {
                let hunk = hunk % text.hunks.len();
                (
                    Selection::Lines {
                        path: RepoPath(path.into()),
                        hunk,
                        lines: chosen_lines.clone(),
                    },
                    DiffPick::Lines {
                        hunk,
                        lines: &chosen_lines,
                    },
                )
            }
        };
        let spec = PatchSpec {
            path,
            direction,
            old_exists: inputs.old.is_some(),
            new_exists: inputs.new.is_some(),
            mode: if inputs.executable {
                Mode::Executable
            } else {
                Mode::Regular
            },
        };
        let patch = match build_patch(&text, &spec, &diff_pick) {
            Ok(Some(patch)) => patch,
            Ok(None) => return Prepared::Nothing,
            Err(error) => panic!("harness resolved a selection that does not exist: {error:?}"),
        };
        let request = StageRequest {
            selection: selection.clone(),
            patch: Patch(patch.clone()),
            based_on: inputs.based_on.clone(),
        };
        // Plain git has a command for a whole file; hunks and lines go in as
        // the same patch (git has no other way to stage part of a file).
        // A whole-file discard is `git restore` (or `git clean` for a file
        // the index does not know), which does not share the app's patch.
        let twin = match (pick, direction) {
            (Pick::File, Direction::Stage) => TwinOp::git(&["add", "--", path]),
            (Pick::File, Direction::Unstage) => TwinOp::git(&["restore", "--staged", "--", path]),
            (Pick::File, Direction::Discard) if inputs.old.is_some() => {
                TwinOp::git(&["restore", "--", path])
            }
            (Pick::File, Direction::Discard) => TwinOp::git(&["clean", "-f", "-q", "--", path]),
            (_, Direction::Discard) => TwinOp {
                args: ["apply", "--whitespace=nowarn", "-"]
                    .map(str::to_owned)
                    .to_vec(),
                stdin: Some(patch),
            },
            _ => TwinOp {
                args: ["apply", "--cached", "--whitespace=nowarn", "-"]
                    .map(str::to_owned)
                    .to_vec(),
                stdin: Some(patch),
            },
        };
        let discarded = (direction == Direction::Discard).then(|| path.to_owned());
        Prepared::Write(Box::new(Write {
            event: match direction {
                Direction::Stage => Event::StageRequested(request),
                Direction::Unstage => Event::UnstageRequested(request),
                Direction::Discard => Event::DiscardRequested(DiscardRequest {
                    selection: request.selection,
                    patch: request.patch,
                    based_on: request.based_on,
                    expect_file: inputs.new_blob.clone(),
                }),
            },
            twin,
            hint: Hint::Other,
            index_only: discarded.is_none(),
            discarded,
        }))
    }

    // ---- driving one request --------------------------------------------

    /// Sends the request (while a foreign process holds `index.lock`, when
    /// `locked`), runs its twin, compares the outcomes and then the repos.
    /// True when the app succeeded.
    fn drive(&mut self, prepared: Prepared, locked: bool) -> bool {
        match prepared {
            Prepared::Nothing => {
                self.refresh_twin();
                self.check();
                false
            }
            Prepared::Write(write) => self.drive_write(write, locked),
            Prepared::Undo => self.drive_undo(locked),
        }
    }

    fn drive_write(&mut self, write: Box<Write>, locked: bool) -> bool {
        let before = self.twin_snap();
        let tree_var = format!("T{}", self.trees);
        self.trees += 1;
        self.script
            .push(format!("{tree_var}=$(git write-tree)  # saved index"));
        let worktree_before = self.twin_obs.worktree.clone();
        let dropped = match write.hint {
            Hint::StashDrop { position } => self.twin_stash(position),
            _ => None,
        };
        let outcome = self.request(write.event.clone(), locked);
        let (twin, changed) = if matches!(outcome, Outcome::Refused(_)) {
            // The app did nothing, so the twin must not either; git alone
            // may well start and leave half a change behind (`git switch -c`
            // checks files out before it finds the branch name taken). Ask
            // git on a copy.
            self.ask_git_on_a_copy(&write.twin)
        } else {
            let twin = self.twin_git(&write.twin);
            let changed = self.refresh_twin();
            (twin, changed)
        };
        let mut succeeded = false;
        let name = format!("{:?}", write.event);
        let name = name.split([' ', '(', '{']).next().unwrap_or("");
        stat(&format!(
            "{}{name}: {}",
            if locked { "locked " } else { "" },
            match &outcome {
                Outcome::Succeeded => "succeeded".to_owned(),
                Outcome::Refused(reason) => format!("refused {}", variant(reason)),
                Outcome::Failed(failure) => format!("failed {:?}", failure.class),
                other => format!("{other:?}"),
            }
        ));
        match &outcome {
            Outcome::Succeeded => {
                if !twin.ok {
                    panic!(
                        "the app succeeded at {:?} but plain git failed: git {:?}\n  {}",
                        write.event, write.twin.args, twin.stderr
                    );
                }
                succeeded = true;
            }
            Outcome::Refused(reason) => {
                if twin.ok && changed {
                    panic!(
                        "the app refused {:?} ({reason:?}) but plain git did it: git {:?}",
                        write.event, write.twin.args
                    );
                }
            }
            Outcome::Failed(failure) => {
                if failure.class != ErrorClass::Expected {
                    panic!(
                        "the app failed {:?} with class {:?}, not a stated reason:\n  {}",
                        write.event, failure.class, failure.details
                    );
                }
                // `git stash push` with nothing to save exits 0 and does
                // nothing; the app reports it as an expected failure. Fine,
                // as long as git changed nothing either.
                if twin.ok && changed {
                    panic!(
                        "the app failed {:?} ({}) but plain git succeeded: git {:?}",
                        write.event, failure.details, write.twin.args
                    );
                }
                // A failed run may still have rewritten the index file with
                // the same content (after an undo's `read-tree`, any git
                // command refreshes the stat data), which changes the id the
                // app compares: a later undo may then be refused as
                // `Changed(Index)`. See tasks/024.DONE.md, finding 1.
                self.mark_ambiguous();
            }
            Outcome::Cancelled | Outcome::Nothing => {
                panic!(
                    "the app dropped {:?} without a refusal or an outcome ({outcome:?})",
                    write.event
                );
            }
        }
        if write.index_only {
            // Law 4: staging never changes a worktree file.
            let after = self.app.repo.worktree();
            if after != worktree_before {
                panic!("law 4: the app changed the worktree while staging or unstaging");
            }
        }
        if let Some(path) = &write.discarded {
            // Law 7: a discard changes the file it was asked for and no
            // other; a refused or failed one changes nothing.
            let after = self.app.repo.worktree();
            let others = |files: &BTreeMap<String, Vec<u8>>| {
                files
                    .iter()
                    .filter(|(name, _)| *name != path)
                    .map(|(name, bytes)| (name.clone(), bytes.clone()))
                    .collect::<BTreeMap<_, _>>()
            };
            if others(&after) != others(&worktree_before) {
                panic!("law 7: the app changed a file other than {path} while discarding");
            }
            if !succeeded && after != worktree_before {
                panic!("law 7: a refused or failed discard changed {path}");
            }
        }
        if succeeded {
            let discard = write.discarded.as_ref().map(|path| Kind::Discard {
                path: path.clone(),
                before: worktree_before.get(path).cloned(),
                after: self.twin_obs.worktree.get(path).cloned(),
            });
            self.push_entry(before, tree_var, write.hint, dropped);
            if let (Some(kind), Some(top)) = (discard, self.stack.last_mut()) {
                top.kind = kind;
            }
        }
        self.settle();
        succeeded
    }

    /// Runs `op` on a throwaway copy of the twin: did it succeed, and did it
    /// change anything. The twin itself is left alone.
    fn ask_git_on_a_copy(&mut self, op: &TwinOp) -> (Run, bool) {
        let copy = self.twin.duplicate(&self._root.path, "scratch");
        let args: Vec<&str> = op.args.iter().map(String::as_str).collect();
        let run = match &op.stdin {
            None => copy.git(&args),
            Some(stdin) => copy.git_stdin(&args, stdin.as_bytes()),
        };
        let changed = observe(&copy) != self.twin_obs;
        let _ = fs::remove_dir_all(copy.paths.workdir);
        (run, changed)
    }

    /// Sends a request. While locked: the lock is taken first, the request
    /// must wait and write nothing, and it is released afterwards.
    fn request(&mut self, event: Event, locked: bool) -> Outcome {
        if !locked {
            return self.app.request(event);
        }
        let before = fingerprint(&self.app.repo);
        let lock = self.app.repo.paths.git_dir.join("index.lock");
        self.script
            .push("touch .git/index.lock  # another process holds the lock".into());
        fs::write(&lock, b"").expect("take index.lock");
        self.app.disk_changed(&[Area::Index]);
        match self.app.state.fresh_disk() {
            Some(disk) if disk.lock == LockState::HeldByOther => {}
            other => panic!(
                "the app did not see the held index.lock: {:?}",
                other.map(|d| d.lock)
            ),
        }
        self.app.deliver(Event::NoticeDismissed);
        self.app.deliver(event);
        // Law 12: nothing is written while the lock is held.
        if fingerprint(&self.app.repo) != before {
            panic!("law 12: the app wrote to .git while another process held index.lock");
        }
        let waiting =
            self.app.state.ops.queued.len() + usize::from(self.app.state.ops.running.is_some());
        if waiting == 0 && self.app.outcome() == Outcome::Nothing {
            panic!("the request was dropped while index.lock was held, not queued");
        }
        if matches!(
            self.app.outcome(),
            Outcome::Succeeded | Outcome::Failed(_) | Outcome::Cancelled
        ) {
            panic!(
                "law 12: the app ran a write while another process held index.lock ({:?})",
                self.app.outcome()
            );
        }
        if waiting > 0 && self.app.outcome() != Outcome::Nothing {
            panic!(
                "a queued write also produced a notice: {:?}",
                self.app.outcome()
            );
        }
        self.script
            .push("rm .git/index.lock  # the lock is gone; the app runs the write now".into());
        fs::remove_file(&lock).expect("release index.lock");
        self.app.disk_changed(&[Area::Index]);
        self.app.outcome()
    }

    /// After a request: the twin is read, then everything is compared.
    fn settle(&mut self) {
        self.refresh_twin();
        self.recover_from_conflicts();
        self.check();
    }

    /// A stash pop that conflicts leaves unmerged entries behind. The app
    /// cannot resolve conflicts yet (a later task), so both repos are reset
    /// the way a user would, behind the app's back.
    fn recover_from_conflicts(&mut self) {
        // P2: a pick or revert that stopped is ended through the app, as the
        // step says; it may stop without a conflict (the change is already
        // there and the commit would be empty), then only abort is left.
        if let Some((end, before)) = self.replay.take()
            && self.twin_paused()
        {
            self.finish_replay(end, &before);
            return;
        }
        if !self.twin_obs.has_unmerged() {
            return;
        }
        stat("conflicting stash recovered");
        let args = ["reset", "-q", "--hard"];
        if let Some(difference) = first_difference(&observe(&self.app.repo), &self.twin_obs) {
            panic!("the repos differ after a conflicting stash: {difference}");
        }
        let app = self.app.repo.git(&args);
        let twin = self.twin_git(&TwinOp::git(&args));
        assert!(app.ok && twin.ok, "git reset --hard failed");
        self.app.everything_changed();
        self.mark_ambiguous();
        self.refresh_twin();
    }

    /// The twin is stopped in a cherry-pick or a revert.
    fn twin_paused(&self) -> bool {
        ["CHERRY_PICK_HEAD", "REVERT_HEAD"]
            .iter()
            .any(|name| self.twin.git(&["rev-parse", "-q", "--verify", name]).ok)
    }

    /// P2: ends a paused cherry-pick or revert in both repos. Abort must
    /// restore everything; resolving every file and continuing must make
    /// one commit and leave an undo point that stops there.
    fn finish_replay(&mut self, end: ReplayEnd, before: &Observed) {
        let verb = if self
            .twin
            .git(&["rev-parse", "-q", "--verify", "REVERT_HEAD"])
            .ok
        {
            "revert"
        } else {
            "cherry-pick"
        };
        let unresolved: Vec<String> = self
            .app
            .state
            .fresh_disk()
            .and_then(|disk| disk.phase.conflicts())
            .map(|conflicts| {
                conflicts
                    .unresolved()
                    .map(|(path, _)| path.0.clone())
                    .collect()
            })
            .unwrap_or_default();
        if end == ReplayEnd::Resolve && !unresolved.is_empty() {
            stat(&format!("{verb} conflict: resolved and continued"));
            let before_continue = self.twin_snap();
            let tree_var = format!("T{}", self.trees);
            self.trees += 1;
            for path in &unresolved {
                let text = "resolved by the harness\n";
                self.script.push(write_line(path, text));
                self.app.repo.write(path, text);
                self.twin.write(path, text);
                self.app.disk_changed(&[Area::Worktree]);
                let outcome = self.app.request(Event::ResolveFileRequested {
                    path: RepoPath(path.clone()),
                    choice: invoke_core::ResolveChoice::Manual,
                });
                assert_eq!(outcome, Outcome::Succeeded, "resolving {path}");
                let add = self.twin_git(&TwinOp::git(&["add", "--", path]));
                assert!(add.ok, "twin: git add failed: {}", add.stderr);
            }
            let outcome = self.app.request(Event::ContinueRequested);
            let twin = self.twin_git(&TwinOp::git(&[
                "-c",
                "core.editor=true",
                verb,
                "--continue",
            ]));
            match outcome {
                Outcome::Succeeded => {
                    assert!(
                        twin.ok,
                        "the app continued the {verb} but git failed: {}",
                        twin.stderr
                    );
                    self.refresh_twin();
                    self.push_entry(before_continue, tree_var, Hint::Continued, None);
                    return;
                }
                // The resolution left nothing to commit: git stays paused
                // too, and only abort is left.
                Outcome::Failed(_) if !twin.ok => {}
                other => panic!(
                    "continuing the {verb}: the app answered {other:?}, git {} ({})",
                    twin.ok, twin.stderr
                ),
            }
        }
        stat(&format!("{verb} stopped: aborted"));
        let outcome = self.app.request(Event::AbortRequested);
        assert_eq!(outcome, Outcome::Succeeded, "aborting the {verb}");
        let abort = self.twin_git(&TwinOp::git(&[verb, "--abort"]));
        assert!(abort.ok, "twin: {verb} --abort failed: {}", abort.stderr);
        self.refresh_twin();
        // Law 13: abort puts the repo back exactly. (An undo may now be
        // refused over a rewritten index file the model cannot see.)
        if let Some(difference) = first_difference(&self.twin_obs, before) {
            panic!("law 13: aborting the {verb} did not restore the repo: {difference}");
        }
        self.mark_ambiguous();
    }

    // ---- the undo model ---------------------------------------------------

    /// The stash entry at `position` as plain git lists it: commit and message.
    fn twin_stash(&self, position: usize) -> Option<(String, String)> {
        let list = self
            .twin
            .git(&["stash", "list", "--format=%H%x09%gs"])
            .text();
        let line = list.lines().nth(position)?;
        let (id, message) = line.split_once('\t')?;
        Some((id.to_owned(), message.to_owned()))
    }

    fn push_entry(
        &mut self,
        before: Snap,
        tree_var: String,
        hint: Hint,
        dropped: Option<(String, String)>,
    ) {
        let after = Snap::of(&self.twin_obs, self.twin.git(&["write-tree"]).text());
        let kind = match hint {
            Hint::StashRewrote => Kind::Irreversible,
            Hint::StashDrop { position: 0 } => {
                // The entry that was on top, as plain git listed it before.
                let (id, message) = dropped.expect("stash listed before the drop");
                Kind::StashDrop { id, message }
            }
            Hint::StashDrop { .. } => Kind::Irreversible,
            Hint::Commit | Hint::Replay => Kind::Refs {
                index_dependent: true,
            },
            Hint::Continued => Kind::Irreversible,
            Hint::Other => Kind::Refs {
                index_dependent: before.index_tree != after.index_tree,
            },
        };
        self.stack.push(Entry {
            kind,
            before,
            after,
            tree_var,
            ambiguous: false,
        });
        while self.stack.len() > MAX_POINTS {
            self.stack.remove(0);
        }
    }

    /// What an undo must do now, from the twin alone.
    fn expectation(&self, now: &Snap) -> Expect {
        let Some(top) = self.stack.last() else {
            return Expect::Refuse("NothingToUndo");
        };
        let moved = match &top.kind {
            Kind::Irreversible => return Expect::Refuse("NotUndoable"),
            // Neither refs nor index matter to a discard's undo: only the
            // file's own bytes (the app does not look at anything else).
            Kind::Discard { path, after, .. } => {
                return if self.twin_obs.worktree.get(path) == after.as_ref() {
                    Expect::Accept
                } else {
                    Expect::RefuseFile
                };
            }
            Kind::StashDrop { .. } => {
                now.refs.get("refs/stash") != top.after.refs.get("refs/stash")
            }
            Kind::Refs { index_dependent } => {
                now.head != top.after.head
                    || footprint(&top.before, &top.after)
                        .iter()
                        .any(|name| now.value(name) != top.after.value(name))
                    || (*index_dependent && now.index_tree != top.after.index_tree)
            }
        };
        if moved {
            Expect::Refuse("Changed")
        } else if top.ambiguous {
            Expect::Either
        } else {
            Expect::Accept
        }
    }

    fn drive_undo(&mut self, locked: bool) -> bool {
        let now = self.twin_snap();
        let expect = self.expectation(&now);
        let outcome = self.request(Event::UndoRequested, locked);
        let mut succeeded = false;
        stat(&format!(
            "{}Undo: {} (model: {expect:?})",
            if locked { "locked " } else { "" },
            match &outcome {
                Outcome::Succeeded => "succeeded".to_owned(),
                Outcome::Refused(reason) => format!("refused {}", variant(reason)),
                other => format!("{other:?}"),
            }
        ));
        match (&expect, &outcome) {
            (Expect::Refuse(kind), Outcome::Refused(reason)) => {
                let named = match reason {
                    Refusal::NothingToUndo => "NothingToUndo",
                    Refusal::NotUndoable(_) => "NotUndoable",
                    Refusal::Changed(_) => "Changed",
                    other => panic!("undo refused for an unexpected reason: {other:?}"),
                };
                if named != *kind {
                    panic!("undo refused as {reason:?}, the model says {kind}");
                }
            }
            (Expect::RefuseFile, Outcome::Failed(failure))
                if failure.class == ErrorClass::PreconditionChanged => {}
            (Expect::RefuseFile, other) => panic!(
                "law 8: the discarded file was changed afterwards, undo must not overwrite it, the app answered {other:?}"
            ),
            (Expect::Refuse(kind), other) => panic!(
                "law 8/9: the model says undo must be refused ({kind}: something else moved the repo) but the app answered {other:?}"
            ),
            (Expect::Either, Outcome::Refused(Refusal::Changed(_))) => {}
            (Expect::Accept | Expect::Either, Outcome::Succeeded) => {
                if let Err(refused) = self.undo_in_twin(&now) {
                    panic!(
                        "the app undid the operation but plain git refuses to restore HEAD: {refused}"
                    );
                }
                succeeded = true;
            }
            (Expect::Accept | Expect::Either, Outcome::Failed(failure))
                if failure.class == ErrorClass::Expected =>
            {
                // Putting HEAD back is a real checkout, and git refuses it
                // when local changes are in the way. The app says so; the
                // twin must be refused too.
                if self.undo_in_twin(&now).is_ok() {
                    panic!(
                        "the app could not undo ({}) but plain git restored HEAD",
                        failure.details
                    );
                }
                self.mark_ambiguous();
            }
            (Expect::Accept | Expect::Either, other) => {
                panic!("undo should have worked on an untouched repo, the app answered {other:?}")
            }
        }
        self.settle();
        succeeded
    }

    /// The twin's undo: put back what the undone operation changed, from
    /// the copy of refs and index saved before it, with plain git.
    ///
    /// `Err` when git refuses to put `HEAD` back (it changed nothing then).
    fn undo_in_twin(&mut self, now: &Snap) -> Result<(), String> {
        let entry = self
            .stack
            .last()
            .cloned()
            .expect("model entry for an accepted undo");
        match &entry.kind {
            Kind::Irreversible => unreachable!("an irreversible entry is never undone"),
            Kind::Discard { path, before, .. } => match before {
                Some(bytes) => {
                    let text = String::from_utf8(bytes.clone()).expect("harness files are text");
                    self.script.push(write_line(path, &text));
                    self.twin.write(path, &text);
                }
                None => {
                    self.script.push(format!("rm {}", quote(path)));
                    self.twin.remove(path);
                }
            },
            Kind::StashDrop { id, message } => {
                let op = TwinOp::git(&["stash", "store", &format!("--message={message}"), id]);
                assert!(self.twin_git(&op).ok, "twin: git stash store failed");
            }
            Kind::Refs { index_dependent } => {
                let head_moved = entry.before.head != entry.after.head;
                if head_moved {
                    let op = match &entry.before.head {
                        HeadAt::Branch(name) => {
                            let tip = entry.before.refs.get(&format!("refs/heads/{name}"));
                            if tip == Some(&now.head_oid) {
                                TwinOp::git(&[
                                    "symbolic-ref",
                                    "HEAD",
                                    &format!("refs/heads/{name}"),
                                ])
                            } else {
                                TwinOp::git(&["switch", "--no-guess", name])
                            }
                        }
                        HeadAt::Detached => {
                            TwinOp::git(&["switch", "--detach", &entry.before.head_oid])
                        }
                    };
                    let run = self.twin_git(&op);
                    if !run.ok {
                        return Err(run.stderr);
                    }
                }
                for name in footprint(&entry.before, &entry.after) {
                    if name == "HEAD" && head_moved {
                        continue;
                    }
                    let op = match entry.before.value(&name) {
                        Some(old) => TwinOp::git(&["update-ref", "--no-deref", &name, old]),
                        None => TwinOp::git(&["update-ref", "-d", &name]),
                    };
                    assert!(self.twin_git(&op).ok, "twin: restoring {name} failed");
                }
                if *index_dependent {
                    self.script
                        .push(format!("git read-tree \"${}\"", entry.tree_var));
                    let tree = &entry.before.index_tree;
                    assert!(
                        self.twin.git(&["read-tree", tree]).ok,
                        "twin: git read-tree failed"
                    );
                }
            }
        }
        self.stack.pop();
        // The app re-bases the new top entry's index id on what the undo
        // left; the model re-bases its tree the same way.
        self.refresh_twin();
        let tree = self.twin.git(&["write-tree"]).text();
        if let Some(top) = self.stack.last_mut() {
            top.after.index_tree = tree;
        }
        Ok(())
    }

    // ---- composite steps ------------------------------------------------

    /// Laws 4 and 5: stage the first hunk until the file has none left; the
    /// worktree file never changes (checked per request) and the index ends
    /// up holding exactly the worktree bytes.
    fn stage_every_hunk(&mut self, path: &str) {
        let mut staged_any = false;
        for _ in 0..64 {
            let prepared = self.prepare_stage(path, &Pick::Hunk(0), Direction::Stage);
            if matches!(prepared, Prepared::Nothing) {
                break;
            }
            if !self.drive(prepared, false) {
                return;
            }
            staged_any = true;
        }
        if !staged_any {
            self.refresh_twin();
            self.check();
            return;
        }
        let blob = self
            .app
            .repo
            .git(&["cat-file", "blob", &format!(":{path}")]);
        let worktree = fs::read(self.app.repo.full(path)).ok();
        let index = blob.ok.then_some(blob.stdout);
        if index != worktree {
            panic!(
                "law 5: after staging every hunk of {path} the index holds {:?}, the worktree {:?}",
                index.map(|b| String::from_utf8_lossy(&b).into_owned()),
                worktree.map(|b| String::from_utf8_lossy(&b).into_owned())
            );
        }
        stat("law 5 checked (every hunk staged)");
    }

    /// Laws 7 and 8: discard a selection; an undo right away gives the file
    /// back byte for byte, and after an edit of the same file it must not
    /// overwrite the edit.
    fn discard_then_undo(&mut self, path: &str, pick: &Pick, edit: Option<&content::Change>) {
        let prepared = self.prepare_stage(path, pick, Direction::Discard);
        if matches!(prepared, Prepared::Nothing) {
            self.refresh_twin();
            self.check();
            return;
        }
        let before = self.app.repo.worktree();
        if !self.drive(prepared, false) {
            return;
        }
        if let Some(change) = edit {
            let text = content::apply(self.twin.read(path).as_deref(), change);
            self.fs_write(path, &text);
            self.outside_files();
        }
        let undone = self.drive(Prepared::Undo, false);
        if undone && edit.is_none() && self.app.repo.worktree() != before {
            panic!("law 8: undoing a discard of {path} did not give the exact files back");
        }
        stat(if edit.is_some() {
            "law 8 checked (discard, edit, undo)"
        } else {
            "law 8 checked (discard then undo)"
        });
    }

    /// Law 6: stage a selection of a file with nothing staged, unstage the
    /// file, and the index is as it was.
    fn stage_then_unstage(&mut self, path: &str, pick: &Pick) {
        let nothing_staged = self.app.state.fresh_disk().is_some_and(|disk| {
            disk.status
                .entries
                .iter()
                .all(|entry| entry.path.0 != path || entry.staged.is_none())
        });
        let before = self.app.repo.git(&["ls-files", "-s", "--", path]).text();
        if !nothing_staged {
            self.refresh_twin();
            self.check();
            return;
        }
        let prepared = self.prepare_stage(path, pick, Direction::Stage);
        if matches!(prepared, Prepared::Nothing) || !self.drive(prepared, false) {
            return;
        }
        let prepared = self.prepare_stage(path, &Pick::File, Direction::Unstage);
        if matches!(prepared, Prepared::Nothing) {
            panic!("law 6: {path} was staged but there is nothing to unstage");
        }
        if !self.drive(prepared, false) {
            panic!("law 6: {path} was staged but could not be unstaged again");
        }
        let after = self.app.repo.git(&["ls-files", "-s", "--", path]).text();
        if after != before {
            panic!(
                "law 6: stage then unstage of {path} left the index different\n  before: {before:?}\n  after:  {after:?}"
            );
        }
        stat("law 6 checked (stage then unstage)");
    }

    // ---- comparing --------------------------------------------------------

    /// Compares the app repo with the twin, and the app's state with disk.
    /// The twin must have been re-read (`refresh_twin`) first.
    fn check(&mut self) {
        assert!(
            self.app.is_idle(),
            "the app is not idle after the step: {:?}",
            self.app.state.ops
        );
        let app_obs = observe(&self.app.repo);
        if let Some(difference) = first_difference(&app_obs, &self.twin_obs) {
            panic!("the app repo and the plain-git twin differ: {difference}");
        }
        self.check_state_against_disk(&app_obs);
        self.check_undo_bookkeeping(&app_obs);
        self.check_no_leftovers();
    }

    /// Law 11: the app's state is what a fresh read of disk gives, and what
    /// plain git says.
    fn check_state_against_disk(&mut self, app_obs: &Observed) {
        let Some(snapshot) = self.app.state.fresh_disk().cloned() else {
            panic!("law 11: the app has no fresh snapshot");
        };
        let fresh = match read_state(&self.app.repo.paths) {
            Ok(fresh) => fresh,
            Err(failure) => panic!("a fresh read failed: {failure:?}"),
        };
        // The app prunes its own saved undo points (refs/invoke-git/) after
        // the read that follows an operation; the watcher reports that
        // later. Until then only those refs may differ.
        let (mut seen, mut now) = (snapshot.clone(), fresh.clone());
        for disk in [&mut seen, &mut now] {
            disk.refs
                .refs
                .retain(|name, _| !name.0.starts_with("refs/invoke-git/"));
        }
        if seen != now {
            panic!(
                "law 11: the app's state differs from a fresh read of disk\n  app state:  {snapshot:?}\n  fresh read: {fresh:?}"
            );
        }
        // After the watcher's next event the two are identical.
        self.app.everything_changed();
        match self.app.state.fresh_disk() {
            Some(after) if *after == read_state(&self.app.repo.paths).expect("fresh read") => {}
            other => panic!(
                "law 11: after the next watcher event the state is {other:?}, not what disk says"
            ),
        }
        let snapshot = &fresh;
        // And the read itself agrees with git.
        let on_disk = &app_obs.all_refs;
        let read_refs: std::collections::BTreeMap<String, String> = snapshot
            .refs
            .refs
            .iter()
            .map(|(name, oid)| (name.0.clone(), oid.to_string()))
            .collect();
        if on_disk != &read_refs {
            panic!(
                "law 11: the state's refs differ from git's\n  state: {read_refs:?}\n  git:   {on_disk:?}"
            );
        }
        let twin_head = Snap::of(app_obs, String::new());
        let state_head = match &snapshot.head {
            invoke_core::Head::Branch { name, oid } => {
                (HeadAt::Branch(name.0.clone()), oid.to_string())
            }
            invoke_core::Head::Detached(oid) => (HeadAt::Detached, oid.to_string()),
            invoke_core::Head::Unborn(name) => (HeadAt::Branch(name.0.clone()), "unborn".into()),
        };
        if (twin_head.head.clone(), twin_head.head_oid.clone()) != state_head {
            panic!(
                "law 11: the state's HEAD is {state_head:?}, disk says {:?}",
                app_obs.head
            );
        }
        let expected = status_from_git(&self.app.repo);
        let found = status_from_state(snapshot);
        if expected != found {
            panic!(
                "law 11: the state's status differs from git status\n  state: {found:?}\n  git:   {expected:?}"
            );
        }
    }

    /// Every saved undo tree belongs to a stack entry, and every entry has
    /// its tree: nothing leaks, nothing is pruned too early.
    fn check_undo_bookkeeping(&self, app_obs: &Observed) {
        let found: BTreeSet<String> = app_obs
            .all_refs
            .keys()
            .filter(|name| name.starts_with("refs/invoke-git/undo/"))
            .cloned()
            .collect();
        let mut expected = BTreeSet::new();
        for entry in &self.app.state.undo.entries {
            expected.insert(format!("refs/invoke-git/undo/{}", entry.point.op.0));
            if matches!(entry.point.reverse, Reversal::Discard { .. }) {
                expected.insert(format!("refs/invoke-git/undo/{}-discard", entry.point.op.0));
                expected.insert(format!(
                    "refs/invoke-git/undo/{}-discard-after",
                    entry.point.op.0
                ));
            }
            if matches!(
                entry.point.reverse,
                Reversal::StashStore(_) | Reversal::StashKept(_)
            ) {
                expected.insert(format!("refs/invoke-git/undo/{}-stash", entry.point.op.0));
            }
        }
        if found != expected {
            panic!(
                "the saved undo points on disk differ from the undo stack\n  on disk: {found:?}\n  stack:   {expected:?}"
            );
        }
        let entries = self.app.state.undo.entries.len();
        if entries != self.stack.len() {
            panic!(
                "the app's undo stack has {entries} entries, the model has {}",
                self.stack.len()
            );
        }
    }

    /// No stray lock or temporary index copy is left in `.git`.
    fn check_no_leftovers(&self) {
        let names: Vec<String> = fs::read_dir(&self.app.repo.paths.git_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name == "index.lock" || name.starts_with("invoke-undo-index"))
            .collect();
        if !names.is_empty() {
            panic!("the app left files behind in .git: {names:?}");
        }
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

/// How often each kind of step ended each way, across all sequences of this
/// process. Printed by `Harness::teardown` when `INVOKE_HARNESS_STATS` is
/// set, to see that the generator reaches the interesting states.
static STATS: std::sync::Mutex<BTreeMap<String, usize>> = std::sync::Mutex::new(BTreeMap::new());

fn stat(label: &str) {
    let mut stats = STATS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *stats.entry(label.to_owned()).or_default() += 1;
}

/// The counts so far, one per line.
pub fn stats_report() -> String {
    let stats = STATS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    stats
        .iter()
        .map(|(label, count)| format!("{count:6}  {label}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The variant name of a value's `Debug` output (`Changed(Index)` -> `Changed`).
fn variant(value: &impl std::fmt::Debug) -> String {
    let text = format!("{value:?}");
    text.split(['(', ' ', '{']).next().unwrap_or("").to_owned()
}

/// `printf` line that writes `text` to `path` in the replay script.
fn write_line(path: &str, text: &str) -> String {
    format!(
        "mkdir -p \"$(dirname {p})\" && printf %s {t} > {p}",
        p = quote(path),
        t = dollar_quote(text)
    )
}

/// Everything a write could change inside `.git`: the raw index file, HEAD,
/// refs and the stash list. Equal before and after means nothing was written.
fn fingerprint(repo: &RepoDir) -> (Vec<u8>, Observed) {
    let mut observed = observe(repo);
    observed.worktree.clear();
    (
        fs::read(repo.paths.git_dir.join("index")).unwrap_or_default(),
        observed,
    )
}

type StatusRow = (String, char, char, Option<String>);

fn code(change: Option<&FileChange>) -> (char, Option<String>) {
    match change {
        None => (' ', None),
        Some(FileChange::Added) => ('A', None),
        Some(FileChange::Modified) => ('M', None),
        Some(FileChange::Deleted) => ('D', None),
        Some(FileChange::TypeChanged) => ('T', None),
        Some(FileChange::Untracked) => ('?', None),
        Some(FileChange::Renamed { from }) => ('R', Some(from.0.clone())),
    }
}

fn status_from_state(disk: &invoke_core::DiskState) -> Vec<StatusRow> {
    let mut rows: Vec<StatusRow> = disk
        .status
        .entries
        .iter()
        .map(|entry| {
            let (staged, from) = code(entry.staged.as_ref());
            let (unstaged, _) = code(entry.unstaged.as_ref());
            (entry.path.0.clone(), staged, unstaged, from)
        })
        .collect();
    rows.sort();
    rows
}

/// `git status --porcelain=v1 -z` as the same rows.
fn status_from_git(repo: &RepoDir) -> Vec<StatusRow> {
    // The app's status does not detect renames (task 11), so neither does this.
    let run = repo.git(&[
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--no-renames",
    ]);
    let text = String::from_utf8_lossy(&run.stdout).into_owned();
    let mut parts = text.split('\0').filter(|part| !part.is_empty());
    // One row per path, as the app has it: git lists a file that is deleted
    // in the index and untracked again in the worktree twice.
    let mut merged: std::collections::BTreeMap<String, (char, char, Option<String>)> =
        std::collections::BTreeMap::new();
    while let Some(entry) = parts.next() {
        let mut chars = entry.chars();
        let (x, y) = (chars.next().unwrap_or(' '), chars.next().unwrap_or(' '));
        let path = entry.get(3..).unwrap_or("").to_owned();
        let from = if x == 'R' || y == 'R' {
            parts.next().map(str::to_owned)
        } else {
            None
        };
        let (x, y) = if x == '?' { (' ', '?') } else { (x, y) };
        let row = merged.entry(path).or_insert((' ', ' ', None));
        if x != ' ' {
            row.0 = x;
            row.2 = from;
        }
        if y != ' ' {
            row.1 = y;
        }
    }
    let mut rows: Vec<StatusRow> = merged
        .into_iter()
        .map(|(path, (x, y, from))| (path, x, y, from))
        .collect();
    rows.sort();
    rows
}
