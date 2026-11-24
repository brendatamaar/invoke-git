//! Finding the system git, and turning the core's typed `GitArgs` into argv.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::git_discovery::{self, GitSource};
use invoke_core::{GitArgs, Operation, PullMode, RepoPath, ResetMode, Side};

/// The oldest git Invoke Git supports (PRD → "Decisions").
pub const MIN_VERSION: GitVersion = GitVersion {
    major: 2,
    minor: 40,
    patch: 0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

/// Why the system git cannot be used. Shown on the blocking setup screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// git could not be started at all.
    Missing(String),
    /// `git --version` printed something we do not understand.
    Unparseable(String),
    TooOld(GitVersion),
    /// The system git is unusable and so is the bundled MinGit (P11).
    FallbackFailed {
        system: Box<SetupError>,
        bundled: Box<SetupError>,
    },
}

/// A usable system git.
#[derive(Clone, Debug)]
pub struct Git {
    program: PathBuf,
    version: GitVersion,
    source: GitSource,
}

/// Parses `git --version` output, for example `git version 2.45.1.windows.1`
/// or `git version 2.39.3 (Apple Git-146)`.
pub fn parse_version(output: &str) -> Option<GitVersion> {
    let rest = output.trim().strip_prefix("git version ")?;
    let mut numbers = rest
        .split(|c: char| c == '.' || c.is_whitespace())
        .map(|part| part.parse::<u32>());
    let major = numbers.next()?.ok()?;
    let minor = numbers.next()?.ok()?;
    // A missing or non-numeric patch (`2.41.rc0`) counts as 0.
    let patch = numbers.next().and_then(Result::ok).unwrap_or(0);
    Some(GitVersion {
        major,
        minor,
        patch,
    })
}

impl Git {
    /// Runs `<program> --version` and checks it against [`MIN_VERSION`].
    pub fn detect(program: impl Into<PathBuf>) -> Result<Self, SetupError> {
        let program = program.into();
        let mut command = Command::new(&program);
        command
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        hide_console(&mut command);
        let output = command
            .output()
            .map_err(|error| SetupError::Missing(format!("{}: {error}", program.display())))?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            return Err(SetupError::Missing(format!(
                "{} --version failed: {}",
                program.display(),
                output.status
            )));
        }
        let version = parse_version(&text).ok_or(SetupError::Unparseable(text))?;
        if version < MIN_VERSION {
            return Err(SetupError::TooOld(version));
        }
        Ok(Self {
            program,
            version,
            source: GitSource::System,
        })
    }

    /// System `git` from PATH when it is new enough; otherwise the MinGit
    /// bundled next to the app executable, if one is installed (P11).
    pub fn detect_with_fallback() -> Result<Self, SetupError> {
        let bundled = std::env::current_exe()
            .ok()
            .map(|exe| git_discovery::bundled_path(&exe));
        Self::detect_with_fallback_at(Path::new("git"), bundled.as_deref())
    }

    /// [`detect_with_fallback`](Self::detect_with_fallback) with the system
    /// program and the bundled path given (tests use scripts).
    pub fn detect_with_fallback_at(
        system: &Path,
        bundled: Option<&Path>,
    ) -> Result<Self, SetupError> {
        let resolved = git_discovery::resolve(
            system,
            bundled,
            &|program| Self::detect(program).map(|git| git.version),
            &|path| path.is_file(),
        )?;
        Ok(Self {
            program: resolved.program,
            version: resolved.version,
            source: resolved.source,
        })
    }

    /// Whether this is the system git or the bundled fallback.
    pub fn source(&self) -> GitSource {
        self.source
    }

    pub fn version(&self) -> GitVersion {
        self.version
    }

    /// A git command in `workdir`, with the user's environment inherited and
    /// only the additions listed in [`ENV`].
    pub fn command(&self, workdir: &Path) -> Command {
        let mut command = Command::new(&self.program);
        command.current_dir(workdir).envs(ENV);
        hide_console(&mut command);
        command
    }
}

/// Environment added to every git run. `GIT_TERMINAL_PROMPT=0` keeps git
/// from blocking on a hidden terminal; `LANGUAGE=en` keeps messages in
/// English so stderr can be classified (it changes message text only).
pub const ENV: [(&str, &str); 2] = [("GIT_TERMINAL_PROMPT", "0"), ("LANGUAGE", "en")];

/// Options placed before every subcommand. Output formatting only; nothing
/// that changes what git does.
pub const GLOBAL_ARGS: [&str; 4] = ["-c", "core.quotepath=false", "-c", "color.ui=false"];

/// A git run: argv after the program name, and what to write to stdin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation {
    pub args: Vec<OsString>,
    pub stdin: Option<Vec<u8>>,
}

pub fn invocation(args: &GitArgs) -> Invocation {
    let mut argv: Vec<OsString> = GLOBAL_ARGS.iter().map(OsString::from).collect();
    match args {
        GitArgs::Commit { message, amend } => {
            // The message goes through stdin: no argv quoting or length limits
            // on Windows, and no message text can be mistaken for an option.
            // Task 14: the core already cleaned the message. Asking for the
            // same cleanup keeps git from doing its own (`commit.cleanup=
            // strip` would delete lines that start with `#`) and the
            // editor's text is exactly what is committed.
            argv.extend(["commit", "--file=-", "--cleanup=whitespace"].map(OsString::from));
            if *amend {
                argv.push("--amend".into());
            }
            Invocation {
                args: argv,
                stdin: Some(message.as_bytes().to_vec()),
            }
        }
        // Task 15. Names were parsed by the core (no leading dash, no
        // spaces), starting points are object ids, so nothing here can be
        // read as an option. `--no-guess` keeps `switch` from inventing a
        // tracking branch out of a remote one: the core named a local one.
        GitArgs::CreateBranch { name, at, switch } => {
            if *switch {
                argv.extend(["switch", "-c"].map(OsString::from));
            } else {
                argv.extend(["branch", "--no-track"].map(OsString::from));
            }
            argv.push(name.0.clone().into());
            argv.push(at.to_string().into());
            Invocation {
                args: argv,
                stdin: None,
            }
        }
        GitArgs::SwitchBranch { name, .. } => {
            argv.extend(["switch", "--no-guess"].map(OsString::from));
            argv.push(name.0.clone().into());
            Invocation {
                args: argv,
                stdin: None,
            }
        }
        GitArgs::RenameBranch { from, to, .. } => {
            argv.extend(["branch", "-m"].map(OsString::from));
            argv.push(from.0.clone().into());
            argv.push(to.0.clone().into());
            Invocation {
                args: argv,
                stdin: None,
            }
        }
        GitArgs::DeleteBranch { name, force, .. } => {
            argv.push("branch".into());
            argv.push(if *force { "-D" } else { "-d" }.into());
            argv.push(name.0.clone().into());
            Invocation {
                args: argv,
                stdin: None,
            }
        }
        // P3: tags. `--no-sign` keeps a `tag.gpgSign` setting from asking
        // for a key (signed tags are out of scope). An annotated message
        // goes through stdin, already cleaned by the core. The names were
        // parsed by the core and the start is an object id; `--` ends the
        // options anyway.
        GitArgs::CreateTag { name, at, message } => {
            argv.extend(["tag", "--no-sign"].map(OsString::from));
            let stdin = message.as_ref().map(|message| {
                argv.extend(["--annotate", "--file=-", "--cleanup=whitespace"].map(OsString::from));
                message.as_bytes().to_vec()
            });
            argv.push("--".into());
            argv.push(name.0.clone().into());
            argv.push(at.to_string().into());
            Invocation { args: argv, stdin }
        }
        GitArgs::DeleteTag { name, .. } => {
            argv.extend(["tag", "--delete", "--"].map(OsString::from));
            argv.push(name.0.clone().into());
            plain(argv)
        }
        // The refspec has no `+`: the remote refuses to move an existing tag.
        GitArgs::PushTag { remote, name, .. } => {
            argv.extend(["push", "--progress", "--"].map(OsString::from));
            argv.push(remote.into());
            argv.push(format!("{0}:{0}", name.to_ref().0).into());
            plain(argv)
        }
        GitArgs::DeleteRemoteTag { remote, name } => {
            argv.extend(["push", "--progress", "--"].map(OsString::from));
            argv.push(remote.into());
            argv.push(format!(":{}", name.to_ref().0).into());
            plain(argv)
        }
        // Task 20: stash. The message is one argv element of the form
        // `--message=<text>`, so it can never be taken for an option. The
        // entry is addressed as `stash@{n}`; the runner has checked that
        // the entry at `n` has the id the core asked for.
        GitArgs::StashSave {
            message,
            include_untracked,
        } => {
            argv.extend(["stash", "push"].map(OsString::from));
            if *include_untracked {
                argv.push("--include-untracked".into());
            }
            if !message.is_empty() {
                argv.push(format!("--message={message}").into());
            }
            plain(argv)
        }
        GitArgs::StashApply { index, .. } => stash_entry(argv, "apply", *index),
        GitArgs::StashPop { index, .. } => stash_entry(argv, "pop", *index),
        GitArgs::StashDrop { index, .. } => stash_entry(argv, "drop", *index),
        // Task 17. `--` ends the options, so no remote or branch name can be
        // taken for one. Nothing here builds `--force` or a lease without its
        // expected value (law 10).
        GitArgs::Fetch { remote } => {
            argv.extend(["fetch", "--progress", "--"].map(OsString::from));
            argv.push(remote.into());
            plain(argv)
        }
        GitArgs::Pull { from, mode } => {
            argv.extend(["pull", "--progress"].map(OsString::from));
            argv.extend(
                match mode {
                    // `--no-edit`: never open an editor for the merge message.
                    PullMode::Merge => ["--no-rebase", "--no-edit"],
                    PullMode::Rebase => ["--rebase", "--no-autostash"],
                }
                .map(OsString::from),
            );
            argv.push("--".into());
            argv.push((&from.remote).into());
            argv.push(from.remote_ref().0.into());
            plain(argv)
        }
        GitArgs::Push {
            local,
            to,
            lease,
            set_upstream,
        } => {
            argv.extend(["push", "--progress"].map(OsString::from));
            if let Some(lease) = lease {
                argv.push(
                    format!(
                        "--force-with-lease={}:{}",
                        lease.remote_ref.0, lease.expected
                    )
                    .into(),
                );
            }
            if *set_upstream {
                argv.push("--set-upstream".into());
            }
            argv.push("--".into());
            argv.push((&to.remote).into());
            // Explicit refspec without `+`: the remote decides whether it is
            // a fast-forward, and the lease alone decides about force.
            argv.push(format!("{}:{}", local.to_ref().0, to.remote_ref().0).into());
            plain(argv)
        }
        GitArgs::SetUpstream { local, upstream } => {
            argv.extend(["branch"].map(OsString::from));
            argv.push(
                format!(
                    "--set-upstream-to={}/{}",
                    upstream.remote, upstream.branch.0
                )
                .into(),
            );
            argv.push("--".into());
            argv.push((&local.0).into());
            plain(argv)
        }
        // Task 23. `--literal-pathspecs` makes the path one literal file, so
        // a name with `*`, `?` or `:(` is never read as a pattern. Taking a
        // side is two runs (see `invocations`); this is the first.
        GitArgs::ResolveTakeSide { path, side } => {
            argv.push("--literal-pathspecs".into());
            argv.extend(["checkout"].map(OsString::from));
            argv.push(
                match side {
                    Side::Ours => "--ours",
                    Side::Theirs => "--theirs",
                }
                .into(),
            );
            argv.push("--".into());
            argv.push((&path.0).into());
            plain(argv)
        }
        // `--force`: the file may have local edits (the conflict markers);
        // the user chose to delete it.
        GitArgs::ResolveRemove { path } => {
            argv.extend(
                ["--literal-pathspecs", "rm", "--force", "--quiet", "--"].map(OsString::from),
            );
            argv.push((&path.0).into());
            plain(argv)
        }
        GitArgs::ResolveStage { path } => stage_path(argv, path),
        GitArgs::InteractiveRebase(plan) => {
            argv.extend(
                [
                    "rebase",
                    "--interactive",
                    "--no-autosquash",
                    "--no-autostash",
                    "--no-update-refs",
                    "--no-rebase-merges",
                    "--no-fork-point",
                    "--reapply-cherry-picks",
                    plan.upstream().as_hex(),
                ]
                .map(OsString::from),
            );
            plain(argv)
        }
        GitArgs::GuardedRebase {
            control,
            step,
            total,
            ..
        } => {
            use invoke_core::rebase::RebaseControl;
            let flag = match control {
                RebaseControl::Continue => "--continue",
                RebaseControl::Skip => "--skip",
                RebaseControl::Abort => "--abort",
            };
            sequencer(
                argv,
                Operation::Rebase {
                    step: *step,
                    total: *total,
                },
                flag,
            )
        }
        GitArgs::Continue { operation } => sequencer(argv, *operation, "--continue"),
        GitArgs::Skip { operation } => sequencer(argv, *operation, "--skip"),
        GitArgs::Abort { operation } => sequencer(argv, *operation, "--abort"),
        // P2. `--no-edit` keeps revert from opening an editor; the commit is
        // an object id and `--` ends the options anyway.
        GitArgs::CherryPick { commit } => {
            argv.extend(["cherry-pick", "--"].map(OsString::from));
            argv.push(commit.to_string().into());
            plain(argv)
        }
        GitArgs::Revert { commit } => {
            argv.extend(["revert", "--no-edit", "--"].map(OsString::from));
            argv.push(commit.to_string().into());
            plain(argv)
        }
        // P7: reflog recovery reset. Only `--soft` and `--mixed` exist so
        // working-tree files are never overwritten and undo (`Reversal::Refs`)
        // restores exact pre-reset state (law 8). Trailing `--` keeps git from
        // reading a 40-hex commit id as a pathspec if a file has that name.
        GitArgs::Reset { target, mode, .. } => {
            argv.push("reset".into());
            argv.push(
                match mode {
                    ResetMode::Soft => "--soft",
                    ResetMode::Mixed => "--mixed",
                }
                .into(),
            );
            argv.push(target.to_string().into());
            argv.push("--".into());
            plain(argv)
        }
    }
}

/// Task 23: every git run of one request, in order; the request stops at the
/// first one that fails. Almost every request is one run. Taking a side
/// whole is two: write the side's file, then stage it.
pub fn invocations(args: &GitArgs) -> Vec<Invocation> {
    match args {
        GitArgs::ResolveTakeSide { path, .. } => {
            let argv = GLOBAL_ARGS.iter().map(OsString::from).collect();
            vec![invocation(args), stage_path(argv, path)]
        }
        _ => vec![invocation(args)],
    }
}

/// `git add -- <path>`. It also stages a deletion when the user removed the
/// file by hand (git 2.x stages removals named explicitly).
fn stage_path(mut argv: Vec<OsString>, path: &RepoPath) -> Invocation {
    argv.extend(["--literal-pathspecs", "add", "--"].map(OsString::from));
    argv.push((&path.0).into());
    plain(argv)
}

/// `git <merge|rebase|cherry-pick|revert> <--continue|--skip|--abort>`.
fn sequencer(mut argv: Vec<OsString>, operation: Operation, flag: &str) -> Invocation {
    argv.push(
        match operation {
            Operation::Merge => "merge",
            Operation::Rebase { .. } => "rebase",
            Operation::CherryPick => "cherry-pick",
            Operation::Revert => "revert",
        }
        .into(),
    );
    argv.push(flag.into());
    plain(argv)
}

fn plain(args: Vec<OsString>) -> Invocation {
    Invocation { args, stdin: None }
}

fn stash_entry(mut argv: Vec<OsString>, verb: &str, index: usize) -> Invocation {
    argv.extend(["stash", verb].map(OsString::from));
    argv.push(format!("stash@{{{index}}}").into());
    plain(argv)
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use invoke_core::{BranchName, Lease, Oid, RefName, RefValue, RemoteBranch};
    use proptest::prelude::*;

    #[test]
    fn versions_from_real_builds_parse() {
        let v = |major, minor, patch| {
            Some(GitVersion {
                major,
                minor,
                patch,
            })
        };
        assert_eq!(parse_version("git version 2.43.0\n"), v(2, 43, 0));
        assert_eq!(parse_version("git version 2.45.1.windows.1"), v(2, 45, 1));
        assert_eq!(
            parse_version("git version 2.39.3 (Apple Git-146)"),
            v(2, 39, 3)
        );
        assert_eq!(parse_version("git version 2.41"), v(2, 41, 0));
        assert_eq!(parse_version("git version 2.48.rc0"), v(2, 48, 0));
        assert_eq!(parse_version("hub version 2.14.2"), None);
        assert_eq!(parse_version("git version x.1"), None);
        assert!(v(2, 39, 9) < Some(MIN_VERSION) && v(2, 40, 0) >= Some(MIN_VERSION));
    }

    #[test]
    fn commit_sends_the_message_on_stdin() {
        let run = invocation(&GitArgs::Commit {
            message: "--amend\n\nbody".into(),
            amend: false,
        });
        let args: Vec<_> = run.args.iter().map(|arg| arg.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "-c",
                "core.quotepath=false",
                "-c",
                "color.ui=false",
                "commit",
                "--file=-",
                "--cleanup=whitespace"
            ]
        );
        assert_eq!(run.stdin.as_deref(), Some(&b"--amend\n\nbody"[..]));
        let amend = invocation(&GitArgs::Commit {
            message: String::new(),
            amend: true,
        });
        assert_eq!(amend.args.last().unwrap(), "--amend");
    }

    fn argv(args: &GitArgs) -> Vec<String> {
        invocation(args)
            .args
            .iter()
            .skip(GLOBAL_ARGS.len())
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn branch_operations_map_to_git_argv() {
        use invoke_core::BranchName;
        let name = |n: &str| BranchName(n.into());
        let at = invoke_core::Oid::from_hex(&"a".repeat(40)).unwrap();
        let line = |text: &str| text.replace("<oid>", &"a".repeat(40));
        let cases = [
            (
                GitArgs::CreateBranch {
                    name: name("x"),
                    at: at.clone(),
                    switch: false,
                },
                line("branch --no-track x <oid>"),
            ),
            (
                GitArgs::CreateBranch {
                    name: name("x"),
                    at: at.clone(),
                    switch: true,
                },
                line("switch -c x <oid>"),
            ),
            (
                GitArgs::SwitchBranch {
                    name: name("x"),
                    tip: at.clone(),
                },
                "switch --no-guess x".to_owned(),
            ),
            (
                GitArgs::RenameBranch {
                    from: name("x"),
                    tip: at.clone(),
                    to: name("y"),
                },
                "branch -m x y".to_owned(),
            ),
            (
                GitArgs::DeleteBranch {
                    name: name("x"),
                    tip: at.clone(),
                    force: false,
                },
                "branch -d x".to_owned(),
            ),
            (
                GitArgs::DeleteBranch {
                    name: name("x"),
                    tip: at,
                    force: true,
                },
                "branch -D x".to_owned(),
            ),
        ];
        for (args, expected) in cases {
            assert_eq!(argv(&args).join(" "), expected);
            assert_eq!(invocation(&args).stdin, None);
        }
    }

    proptest! {
        /// A name the core accepted never starts with a dash, so it is never
        /// read as an option.
        #[test]
        fn parsed_branch_names_are_never_options(text in "\\PC{0,12}") {
            use invoke_core::BranchName;
            if let Ok(name) = BranchName::parse(&text) {
                let at = invoke_core::Oid::from_hex(&"b".repeat(40)).unwrap();
                let run = argv(&GitArgs::RenameBranch { from: name.clone(), tip: at, to: name });
                prop_assert!(!run[2].starts_with('-') && !run[3].starts_with('-'));
            }
        }
    }

    #[test]
    fn stash_argv() {
        let oid = invoke_core::Oid::from_hex(&"a".repeat(40)).unwrap();
        let argv = |args: &GitArgs| -> Vec<String> {
            let run = invocation(args);
            assert_eq!(run.stdin, None);
            run.args[GLOBAL_ARGS.len()..]
                .iter()
                .map(|arg| arg.to_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            argv(&GitArgs::StashSave {
                message: "wip".into(),
                include_untracked: true
            }),
            ["stash", "push", "--include-untracked", "--message=wip"]
        );
        assert_eq!(
            argv(&GitArgs::StashSave {
                message: String::new(),
                include_untracked: false
            }),
            ["stash", "push"]
        );
        let at = |index| (index, oid.clone());
        let (index, stash) = at(3);
        assert_eq!(
            argv(&GitArgs::StashApply { index, stash }),
            ["stash", "apply", "stash@{3}"]
        );
        let (index, stash) = at(0);
        assert_eq!(
            argv(&GitArgs::StashPop { index, stash }),
            ["stash", "pop", "stash@{0}"]
        );
        let (index, stash) = at(12);
        assert_eq!(
            argv(&GitArgs::StashDrop { index, stash }),
            ["stash", "drop", "stash@{12}"]
        );
    }

    proptest! {
        /// A stash message is one argv element that starts with `--message=`.
        #[test]
        fn stash_messages_stay_one_option_value(message in "[^\\x00]{1,40}") {
            let run = invocation(&GitArgs::StashSave {
                message: message.clone(),
                include_untracked: false,
            });
            let last = run.args.last().unwrap().to_str().unwrap().to_owned();
            prop_assert_eq!(last, format!("--message={message}"));
            prop_assert_eq!(run.args.len(), GLOBAL_ARGS.len() + 3);
        }

        /// No message, however odd, ever reaches argv.
        #[test]
        fn messages_never_reach_argv(message in "\\PC*", amend in any::<bool>()) {
            let run = invocation(&GitArgs::Commit { message: message.clone(), amend });
            prop_assert_eq!(run.stdin, Some(message.into_bytes()));
            prop_assert_eq!(run.args.len(), GLOBAL_ARGS.len() + 3 + usize::from(amend));
        }
    }

    // Task 17: law 10, argv for fetch, pull, push and set-upstream.

    fn strings(run: &Invocation) -> Vec<String> {
        run.args
            .iter()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect()
    }

    fn after_globals(run: &Invocation) -> Vec<String> {
        strings(run)[GLOBAL_ARGS.len()..].to_vec()
    }

    fn remote_branch(remote: &str, branch: &str) -> RemoteBranch {
        RemoteBranch {
            remote: remote.into(),
            branch: BranchName(branch.into()),
        }
    }

    #[test]
    fn network_commands_have_the_expected_argv() {
        let oid = Oid::from_hex(&"a".repeat(40)).unwrap();
        let fetch = invocation(&GitArgs::Fetch {
            remote: "origin".into(),
        });
        assert_eq!(
            after_globals(&fetch),
            ["fetch", "--progress", "--", "origin"]
        );
        let merge = invocation(&GitArgs::Pull {
            from: remote_branch("origin", "main"),
            mode: PullMode::Merge,
        });
        assert_eq!(
            after_globals(&merge),
            [
                "pull",
                "--progress",
                "--no-rebase",
                "--no-edit",
                "--",
                "origin",
                "refs/heads/main"
            ]
        );
        let rebase = invocation(&GitArgs::Pull {
            from: remote_branch("origin", "main"),
            mode: PullMode::Rebase,
        });
        assert!(after_globals(&rebase).contains(&"--rebase".to_owned()));
        let push = invocation(&GitArgs::Push {
            local: BranchName("topic".into()),
            to: remote_branch("origin", "topic"),
            lease: Some(Lease {
                remote_ref: RefName("refs/heads/topic".into()),
                expected: oid,
            }),
            set_upstream: true,
        });
        assert_eq!(
            after_globals(&push),
            [
                "push".to_owned(),
                "--progress".to_owned(),
                format!("--force-with-lease=refs/heads/topic:{}", "a".repeat(40)),
                "--set-upstream".to_owned(),
                "--".to_owned(),
                "origin".to_owned(),
                "refs/heads/topic:refs/heads/topic".to_owned(),
            ]
        );
        let plain = invocation(&GitArgs::Push {
            local: BranchName("topic".into()),
            to: remote_branch("origin", "topic"),
            lease: None,
            set_upstream: false,
        });
        assert_eq!(
            after_globals(&plain),
            [
                "push",
                "--progress",
                "--",
                "origin",
                "refs/heads/topic:refs/heads/topic"
            ]
        );
        let upstream = invocation(&GitArgs::SetUpstream {
            local: BranchName("topic".into()),
            upstream: remote_branch("origin", "topic"),
        });
        assert_eq!(
            after_globals(&upstream),
            ["branch", "--set-upstream-to=origin/topic", "--", "topic"]
        );
    }

    fn arb_oid() -> impl Strategy<Value = Oid> {
        prop_oneof![
            "[0-9a-f]{40}".prop_map(|hex| Oid::from_hex(&hex).unwrap()),
            "[0-9a-f]{64}".prop_map(|hex| Oid::from_hex(&hex).unwrap()),
        ]
    }

    fn arb_name() -> impl Strategy<Value = String> {
        // Includes names that look like options or refspecs.
        prop_oneof![
            "[a-z][a-z0-9/_.-]{0,12}",
            Just("-f".to_owned()),
            Just("--force".to_owned()),
            Just("+main".to_owned()),
            Just("--force-with-lease".to_owned()),
        ]
    }

    fn arb_remote_branch() -> impl Strategy<Value = RemoteBranch> {
        (arb_name(), arb_name()).prop_map(|(remote, branch)| remote_branch(&remote, &branch))
    }

    fn arb_args() -> impl Strategy<Value = GitArgs> {
        let lease = prop::option::of((arb_name(), arb_oid()).prop_map(|(name, expected)| Lease {
            remote_ref: RefName(format!("refs/heads/{name}")),
            expected,
        }));
        prop_oneof![
            arb_name().prop_map(|remote| GitArgs::Fetch { remote }),
            (arb_remote_branch(), any::<bool>()).prop_map(|(from, rebase)| GitArgs::Pull {
                from,
                mode: if rebase {
                    PullMode::Rebase
                } else {
                    PullMode::Merge
                },
            }),
            (arb_name(), arb_remote_branch(), lease, any::<bool>()).prop_map(
                |(local, to, lease, set_upstream)| GitArgs::Push {
                    local: BranchName(local),
                    to,
                    lease,
                    set_upstream,
                }
            ),
            (arb_name(), arb_remote_branch()).prop_map(|(local, upstream)| GitArgs::SetUpstream {
                local: BranchName(local),
                upstream
            }),
        ]
    }

    proptest! {
        /// Law 10: the only way to force is a lease with its expected value.
        /// No `--force`, `-f`, bare `--force-with-lease` or `+` refspec is
        /// ever built, and a lease appears exactly when the request has one,
        /// with that value.
        #[test]
        fn force_only_ever_comes_with_an_expected_value(args in arb_args()) {
            let run = invocation(&args);
            let argv = strings(&run);
            // User-chosen names come after `--`.
            let options: Vec<&String> = argv[GLOBAL_ARGS.len()..]
                .iter()
                .take_while(|arg| arg.as_str() != "--")
                .collect();
            for option in &options {
                prop_assert_ne!(option.as_str(), "--force");
                prop_assert_ne!(option.as_str(), "-f");
                prop_assert_ne!(option.as_str(), "--force-with-lease");
            }
            let leases: Vec<&&String> = options
                .iter()
                .filter(|option| option.starts_with("--force-with-lease"))
                .collect();
            match &args {
                GitArgs::Push { lease: Some(lease), .. } => {
                    let expected = format!(
                        "--force-with-lease={}:{}",
                        lease.remote_ref.0, lease.expected
                    );
                    prop_assert_eq!(leases.len(), 1);
                    prop_assert_eq!(leases[0].as_str(), expected.as_str());
                }
                _ => prop_assert!(leases.is_empty()),
            }
            if let GitArgs::Push { .. } = &args {
                let refspec = argv.last().unwrap();
                prop_assert!(!refspec.starts_with('+'), "{refspec}");
            }
            prop_assert!(run.stdin.is_none());
        }

        /// Network commands always ask git for progress.
        #[test]
        fn network_commands_ask_for_progress(args in arb_args()) {
            let argv = strings(&invocation(&args));
            let has = argv.contains(&"--progress".to_owned());
            prop_assert_eq!(has, args.is_network());
        }
    }

    // Task 23: the conflict flow.

    fn all_runs(args: &GitArgs) -> Vec<Vec<String>> {
        invocations(args).iter().map(after_globals).collect()
    }

    fn rel(name: &str) -> RepoPath {
        RepoPath(name.into())
    }

    #[test]
    fn cherry_pick_and_revert_name_the_commit_after_the_options() {
        let commit = invoke_core::Oid::from_hex(&"b".repeat(40)).unwrap();
        let pick = GitArgs::CherryPick {
            commit: commit.clone(),
        };
        assert_eq!(argv(&pick).join(" "), format!("cherry-pick -- {commit}"));
        let revert = GitArgs::Revert {
            commit: commit.clone(),
        };
        assert_eq!(
            argv(&revert).join(" "),
            format!("revert --no-edit -- {commit}")
        );
        assert!(pick.may_open_editor() && revert.may_open_editor());
    }

    #[test]
    fn conflict_commands_have_the_expected_argv() {
        let rebase = Operation::Rebase { step: 2, total: 3 };
        let line = |runs: Vec<Vec<String>>| -> Vec<String> {
            runs.into_iter().map(|run| run.join(" ")).collect()
        };
        for (operation, verb) in [
            (Operation::Merge, "merge"),
            (rebase, "rebase"),
            (Operation::CherryPick, "cherry-pick"),
            (Operation::Revert, "revert"),
        ] {
            assert_eq!(
                line(all_runs(&GitArgs::Continue { operation })),
                [format!("{verb} --continue")]
            );
            assert_eq!(
                line(all_runs(&GitArgs::Abort { operation })),
                [format!("{verb} --abort")]
            );
        }
        assert_eq!(
            line(all_runs(&GitArgs::Skip { operation: rebase })),
            ["rebase --skip"]
        );
        assert_eq!(
            line(all_runs(&GitArgs::ResolveTakeSide {
                path: rel("a b.txt"),
                side: Side::Ours
            })),
            [
                "--literal-pathspecs checkout --ours -- a b.txt",
                "--literal-pathspecs add -- a b.txt"
            ]
        );
        assert_eq!(
            line(all_runs(&GitArgs::ResolveTakeSide {
                path: rel("a.txt"),
                side: Side::Theirs
            })),
            [
                "--literal-pathspecs checkout --theirs -- a.txt",
                "--literal-pathspecs add -- a.txt"
            ]
        );
        assert_eq!(
            line(all_runs(&GitArgs::ResolveRemove { path: rel("a.txt") })),
            ["--literal-pathspecs rm --force --quiet -- a.txt"]
        );
        assert_eq!(
            line(all_runs(&GitArgs::ResolveStage { path: rel("a.txt") })),
            ["--literal-pathspecs add -- a.txt"]
        );
    }

    fn arb_operation() -> impl Strategy<Value = Operation> {
        prop_oneof![
            Just(Operation::Merge),
            (0..9u32, 0..9u32).prop_map(|(step, total)| Operation::Rebase { step, total }),
            Just(Operation::CherryPick),
            Just(Operation::Revert),
        ]
    }

    proptest! {
        /// A path, however odd (a leading dash, a glob, a magic pathspec),
        /// is only ever a literal path after `--`, in every run.
        #[test]
        fn resolve_paths_are_always_literal_after_the_separator(
            name in prop_oneof![
                "[a-z*?\\[\\]:()!-][a-z0-9 *?\\[\\]:()./!-]{0,10}",
                Just("-f".to_owned()),
                Just(":(top)x".to_owned()),
                Just("*.txt".to_owned()),
            ],
            side in prop_oneof![Just(Side::Ours), Just(Side::Theirs)],
        ) {
            for args in [
                GitArgs::ResolveTakeSide { path: rel(&name), side },
                GitArgs::ResolveRemove { path: rel(&name) },
                GitArgs::ResolveStage { path: rel(&name) },
            ] {
                for run in invocations(&args) {
                    let argv = strings(&run);
                    prop_assert!(argv.contains(&"--literal-pathspecs".to_owned()));
                    let separator = argv.iter().position(|arg| arg == "--").unwrap();
                    prop_assert_eq!(&argv[separator + 1..], std::slice::from_ref(&name));
                    // The only options come before the separator.
                    prop_assert_eq!(argv.len(), separator + 2);
                    prop_assert!(run.stdin.is_none());
                }
            }
        }

        /// Only continuing, skipping and aborting may open an editor, and
        /// they take no message or path on the command line.
        #[test]
        fn sequencer_runs_take_no_free_text(operation in arb_operation()) {
            for args in [
                GitArgs::Continue { operation },
                GitArgs::Skip { operation },
                GitArgs::Abort { operation },
            ] {
                prop_assert!(args.may_open_editor());
                let runs = invocations(&args);
                prop_assert_eq!(runs.len(), 1);
                prop_assert_eq!(after_globals(&runs[0]).len(), 2);
                prop_assert!(runs[0].stdin.is_none());
            }
            let stage = GitArgs::ResolveStage { path: rel("a") };
            prop_assert!(!stage.may_open_editor());
        }
    }

    #[test]
    fn reset_invocation_uses_soft_or_mixed_and_trailing_separator() {
        let target = Oid::from_hex(&"a".repeat(40)).unwrap();
        let expected_ref = RefValue {
            name: RefName("refs/heads/main".into()),
            oid: Some(Oid::from_hex(&"b".repeat(40)).unwrap()),
        };
        let soft = invocation(&GitArgs::Reset {
            target: target.clone(),
            mode: ResetMode::Soft,
            expected_ref: expected_ref.clone(),
        });
        assert_eq!(
            after_globals(&soft),
            vec!["reset", "--soft", &target.to_string(), "--"]
        );
        assert!(soft.stdin.is_none());

        let mixed = invocation(&GitArgs::Reset {
            target: target.clone(),
            mode: ResetMode::Mixed,
            expected_ref,
        });
        assert_eq!(
            after_globals(&mixed),
            vec!["reset", "--mixed", &target.to_string(), "--"]
        );
        assert!(mixed.stdin.is_none());
    }
}
