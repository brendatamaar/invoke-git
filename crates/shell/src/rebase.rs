//! Guard and run the interactive rebase editor without a shell.

use std::collections::BTreeSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU64, Ordering};

use invoke_core::detect::detect_phase;
use invoke_core::{
    Conflicts, ErrorClass, Expect, Failure, GitArgs, Oid, Operation, Phase, RebaseAction,
    RebaseOrigin, RebasePlan, RefValue,
};

use crate::precondition::{self, RepoPaths};

const TODO_ENV: &str = "INVOKE_REBASE_TODO_FILE";
const EDITOR: &str = if cfg!(windows) {
    "invoke-rebase-editor.exe"
} else {
    "invoke-rebase-editor"
};

pub(crate) fn default_editor() -> Option<PathBuf> {
    Some(env::current_exe().ok()?.with_file_name(EDITOR))
}

pub(crate) fn is_sequencer(args: &GitArgs) -> bool {
    matches!(
        args,
        GitArgs::InteractiveRebase(_)
            | GitArgs::GuardedRebase { .. }
            | GitArgs::Continue {
                operation: Operation::Rebase { .. }
            }
            | GitArgs::Skip {
                operation: Operation::Rebase { .. }
            }
            | GitArgs::Abort {
                operation: Operation::Rebase { .. }
            }
    )
}

pub(crate) fn check(paths: &RepoPaths, args: &GitArgs, expect: &Expect) -> Result<(), Failure> {
    let (operation, expected_origin) = match args {
        GitArgs::InteractiveRebase(plan) => {
            let phase = detect_phase(
                &crate::read::read_markers(&paths.git_dir),
                Conflicts::default(),
            );
            if phase != Phase::Clean {
                return Err(changed("another Git operation has started"));
            }
            return check_plan(paths, plan, expect);
        }
        GitArgs::GuardedRebase {
            step,
            total,
            origin,
            ..
        } => (
            Operation::Rebase {
                step: *step,
                total: *total,
            },
            Some(origin),
        ),
        GitArgs::Continue {
            operation: operation @ Operation::Rebase { .. },
        }
        | GitArgs::Skip {
            operation: operation @ Operation::Rebase { .. },
        }
        | GitArgs::Abort {
            operation: operation @ Operation::Rebase { .. },
        } => (*operation, None),
        _ => return Ok(()),
    };

    let phase = detect_phase(
        &crate::read::read_markers(&paths.git_dir),
        Conflicts::default(),
    );
    if phase.operation() != Some(operation) {
        return Err(changed("the rebase step changed"));
    }
    let Phase::Rebasing {
        origin: Some(origin),
        ..
    } = phase
    else {
        return Err(changed("the rebase origin could not be read"));
    };
    if expected_origin.is_some_and(|expected| *expected != origin) {
        return Err(changed("the rebase origin changed"));
    }
    if expected_origin.is_none()
        && let Some(branch) = origin.branch
    {
        precondition::check_refs(
            paths,
            &[RefValue {
                name: branch.to_ref(),
                oid: Some(origin.tip),
            }],
        )?;
    }
    Ok(())
}

fn changed(details: &str) -> Failure {
    Failure {
        class: ErrorClass::PreconditionChanged,
        details: details.into(),
    }
}

/// Validate the complete range using Git objects. An incomplete plan or a
/// merge inside the range could remove history that the user never selected.
fn check_plan(paths: &RepoPaths, plan: &RebasePlan, expect: &Expect) -> Result<(), Failure> {
    let invalid = |details: String| Failure {
        class: ErrorClass::Expected,
        details,
    };
    let repo = precondition::open(paths).map_err(invalid)?;
    let mut next = RebaseOrigin::from_head(&expect.head)
        .ok_or_else(|| invalid("interactive rebase needs an existing HEAD".into()))?
        .tip;
    let mut commits: BTreeSet<_> = plan.steps().iter().map(|(commit, _)| commit).collect();
    while &next != plan.upstream() {
        if !commits.remove(&next) {
            return Err(invalid(
                "the plan must cover every commit from HEAD to its upstream".into(),
            ));
        }
        let id = gix::ObjectId::from_hex(next.as_hex().as_bytes())
            .map_err(|error| invalid(error.to_string()))?;
        let commit = repo
            .find_commit(id)
            .map_err(|error| invalid(format!("read {next}: {error}")))?;
        let mut parents = commit.parent_ids();
        next = match (parents.next(), parents.next()) {
            (Some(parent), None) => precondition::oid(&parent).map_err(invalid)?,
            _ => {
                return Err(invalid(
                    "interactive rebase requires a linear range above an upstream commit".into(),
                ));
            }
        };
    }
    if !commits.is_empty() {
        return Err(invalid(
            "the plan contains commits outside the rebase range".into(),
        ));
    }
    Ok(())
}

/// One commit in a linear rebase range, read for the interactive rebase editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebaseCommit {
    pub oid: Oid,
    pub subject: String,
    pub author: String,
}

/// Read every commit strictly above `upstream` up to `head`, oldest first.
/// Refuses non-linear ranges (merges) or commits that are not ancestors of `head`,
/// matching [`check_plan`].
pub fn read_rebase_commits(
    paths: &RepoPaths,
    upstream: &Oid,
    head: &Oid,
) -> Result<Vec<RebaseCommit>, String> {
    if upstream == head {
        return Err("Choose a commit older than HEAD to rebase onto.".into());
    }
    let repo = precondition::open(paths)?;
    let mut next = head.clone();
    let mut commits = Vec::new();
    while &next != upstream {
        let id =
            gix::ObjectId::from_hex(next.as_hex().as_bytes()).map_err(|error| error.to_string())?;
        let commit = repo
            .find_commit(id)
            .map_err(|error| format!("Could not read {next}: {error}"))?;
        let message = commit
            .message()
            .map(|m| m.summary().to_string())
            .unwrap_or_default();
        let author = commit
            .author()
            .map(|a| a.name.to_string())
            .unwrap_or_default();
        commits.push(RebaseCommit {
            oid: next.clone(),
            subject: message,
            author,
        });
        let mut parents = commit.parent_ids();
        next = match (parents.next(), parents.next()) {
            (Some(parent), None) => precondition::oid(&parent)?,
            (None, _) => {
                return Err("The chosen commit is not an ancestor of HEAD.".into());
            }
            (Some(_), Some(_)) => {
                return Err(
                    "Interactive rebase requires a linear range with no merge commits.".into(),
                );
            }
        };
    }
    commits.reverse();
    Ok(commits)
}

/// The todo travels in a file because Windows limits the size of one
/// environment value. PATH contains the helper directory as a native OS
/// path; Git receives only a basename with no shell metacharacters.
pub(crate) struct TodoFile {
    path: PathBuf,
}

impl TodoFile {
    pub(crate) fn prepare(
        command: &mut Command,
        program: &Path,
        plan: &RebasePlan,
    ) -> io::Result<Self> {
        configure_editor(command, program)?;
        let (todo, mut file) = Self::create()?;
        for (commit, action) in plan.steps() {
            let verb = match action {
                RebaseAction::Pick => "pick",
                RebaseAction::Squash => "squash",
                RebaseAction::Fixup => "fixup",
                RebaseAction::Edit => "edit",
                RebaseAction::Drop => "drop",
            };
            writeln!(file, "{verb} {commit}")?;
        }
        command.env(TODO_ENV, &todo.path);
        Ok(todo)
    }

    fn create() -> io::Result<(Self, fs::File)> {
        static NEXT_TODO: AtomicU64 = AtomicU64::new(0);
        let dir = env::temp_dir();
        loop {
            let id = NEXT_TODO.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("invoke-rebase-{}-{id}.todo", std::process::id()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => return Ok((Self { path }, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for TodoFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn configure_editor(command: &mut Command, program: &Path) -> io::Result<()> {
    let name = program
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| {
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
        .ok_or_else(|| io::Error::other("invalid rebase editor executable name"))?;
    if !program.is_absolute() || !program.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "rebase editor executable unavailable at {}",
                program.display()
            ),
        ));
    }
    let inherited = command
        .get_envs()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map_or_else(
            || env::var_os("PATH"),
            |(_, value)| value.map(ToOwned::to_owned),
        );
    let dir = program
        .parent()
        .expect("absolute path to executable has a parent");
    let paths = env::join_paths(
        std::iter::once(dir.to_owned()).chain(inherited.iter().flat_map(env::split_paths)),
    )
    .map_err(io::Error::other)?;
    command.env("PATH", paths).env("GIT_SEQUENCE_EDITOR", name);
    Ok(())
}

#[doc(hidden)]
pub fn editor_main() -> ExitCode {
    match edit_todo() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("invoke rebase editor: {error}");
            ExitCode::FAILURE
        }
    }
}

fn edit_todo() -> io::Result<()> {
    let target = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name == "git-rebase-todo")
        })
        .ok_or_else(|| io::Error::other("expected Git's rebase todo path"))?;
    let source =
        env::var_os(TODO_ENV).ok_or_else(|| io::Error::other("no rebase plan supplied"))?;
    let todo = fs::read(source)?;
    fs::write(target, todo)
}
