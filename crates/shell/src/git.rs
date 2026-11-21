//! Finding the system git, and turning the core's typed `GitArgs` into argv.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use invoke_core::{GitArgs, PullMode};

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
}

/// A usable system git.
#[derive(Clone, Debug)]
pub struct Git {
    program: PathBuf,
    version: GitVersion,
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
        Ok(Self { program, version })
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

impl Invocation {
    fn plain(args: Vec<OsString>) -> Self {
        Self { args, stdin: None }
    }
}

pub fn invocation(args: &GitArgs) -> Invocation {
    let mut argv: Vec<OsString> = GLOBAL_ARGS.iter().map(OsString::from).collect();
    match args {
        GitArgs::Commit { message, amend } => {
            // The message goes through stdin: no argv quoting or length limits
            // on Windows, and no message text can be mistaken for an option.
            argv.extend(["commit", "--file=-"].map(OsString::from));
            if *amend {
                argv.push("--amend".into());
            }
            Invocation {
                args: argv,
                stdin: Some(message.as_bytes().to_vec()),
            }
        }
        // Task 17. `--` ends the options, so no remote or branch name can be
        // taken for one. Nothing here builds `--force` or a lease without its
        // expected value (law 10).
        GitArgs::Fetch { remote } => {
            argv.extend(["fetch", "--progress", "--"].map(OsString::from));
            argv.push(remote.into());
            Invocation::plain(argv)
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
            Invocation::plain(argv)
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
            Invocation::plain(argv)
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
            Invocation::plain(argv)
        }
    }
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
    use invoke_core::{BranchName, Lease, Oid, RefName, RemoteBranch};
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
                "--file=-"
            ]
        );
        assert_eq!(run.stdin.as_deref(), Some(&b"--amend\n\nbody"[..]));
        let amend = invocation(&GitArgs::Commit {
            message: String::new(),
            amend: true,
        });
        assert_eq!(amend.args.last().unwrap(), "--amend");
    }

    proptest! {
        /// No message, however odd, ever reaches argv.
        #[test]
        fn messages_never_reach_argv(message in "\\PC*", amend in any::<bool>()) {
            let run = invocation(&GitArgs::Commit { message: message.clone(), amend });
            prop_assert_eq!(run.stdin, Some(message.into_bytes()));
            prop_assert_eq!(run.args.len(), GLOBAL_ARGS.len() + 2 + usize::from(amend));
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
}
