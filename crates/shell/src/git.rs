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
}
