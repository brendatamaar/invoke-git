//! Finding the system git, and turning the core's typed `GitArgs` into argv.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use invoke_core::GitArgs;

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
    }
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
            prop_assert_eq!(run.args.len(), GLOBAL_ARGS.len() + 2 + usize::from(amend));
        }
    }
}
