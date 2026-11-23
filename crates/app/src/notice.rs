//! What the user is told about the newest event: the core's `Notice` as
//! plain words. Pure. Errors name the cause and the fix; a raw git message
//! only appears as "Details" (README, "Content fundamentals").

use invoke_core::{
    Action, BranchRefusal, CommitRefusal, ConflictRefusal, ErrorClass, Failure, Notice, OpOutcome,
    Refusal, TagRefusal, UndoMoved,
};

/// How to draw a notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tone {
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub tone: Tone,
    pub text: String,
    /// The raw output, shown behind a toggle.
    pub details: Option<String>,
}

fn action_name(action: Action) -> &'static str {
    match action {
        Action::Stage => "Stage",
        Action::Unstage => "Unstage",
        Action::Discard => "Discard",
        Action::Commit => "Commit",
        Action::Push => "Push",
        Action::Undo => "Undo",
        Action::CreateBranch => "Create branch",
        Action::SwitchBranch => "Switch branch",
        Action::RenameBranch => "Rename branch",
        Action::DeleteBranch => "Delete branch",
        Action::CreateTag => "Create tag",
        Action::DeleteTag => "Delete tag",
        Action::PushTag => "Push tag",
        Action::DeleteRemoteTag => "Delete remote tag",
        Action::StashSave => "Stash",
        Action::StashApply => "Apply stash",
        Action::StashPop => "Pop stash",
        Action::StashDrop => "Drop stash",
        Action::Fetch => "Fetch",
        Action::Pull => "Pull",
        Action::SetUpstream => "Set upstream",
        Action::ResolveFile => "Resolve",
        Action::Continue => "Continue",
        Action::Skip => "Skip",
        Action::Abort => "Abort",
        Action::CherryPick => "Cherry-pick",
        Action::Revert => "Revert",
        Action::RemoveLock => "Remove lock",
    }
}

fn refusal_text(reason: &Refusal) -> String {
    match reason {
        Refusal::NotYetSupported { task } => {
            format!("This is not available yet (backlog task {task}).")
        }
        Refusal::NothingToUndo => "There is nothing to undo.".into(),
        Refusal::StaleDiff => {
            "The diff is older than the repository. It has been refreshed; select again.".into()
        }
        Refusal::NothingSelected => {
            "Nothing is selected: the selection has no changed lines.".into()
        }
        Refusal::NotUndoable(_) => {
            "That operation cannot be undone (it changed the remote or rewrote files).".into()
        }
        Refusal::Changed(moved) => format!(
            "Cannot undo: {} changed since. Nothing was touched.",
            match moved {
                UndoMoved::Head => "HEAD".to_owned(),
                UndoMoved::Ref(name) => name.0.clone(),
                UndoMoved::Index => "the index".to_owned(),
            }
        ),
        Refusal::NoStaleLock => {
            "There is no leftover lock to remove, or an operation is still running.".into()
        }
        Refusal::RepoBusy => {
            "A merge, rebase, cherry-pick or revert is in progress. Finish or abort it first."
                .into()
        }
        Refusal::StashGone(_) => "That stash entry is gone. The list has been refreshed.".into(),
        Refusal::Branch(problem) => branch_text(problem),
        Refusal::Tag(problem) => tag_text(problem),
        Refusal::NotRead => "The repository has not been read yet. Try again in a moment.".into(),
        Refusal::NoBranch => "This needs a checked-out branch, and HEAD is detached.".into(),
        Refusal::NoRemoteValue(_) => {
            "Force push needs the remote value you reviewed. Fetch first.".into()
        }
        Refusal::BranchChanged => "The checked-out branch changed. Try again.".into(),
        Refusal::Commit(problem) => commit_text(problem),
        Refusal::Conflict(problem) => conflict_text(problem),
        Refusal::UncommittedChanges => {
            "Commit or stash your changes to tracked files first.".into()
        }
    }
}

fn conflict_text(problem: &ConflictRefusal) -> String {
    match problem {
        ConflictRefusal::NotInProgress => {
            "No merge, rebase, cherry-pick or revert is waiting for this.".into()
        }
        ConflictRefusal::PhaseChanged => {
            "The operation moved on since you looked (for example to the next rebase step). Check the files and try again.".into()
        }
        ConflictRefusal::Unresolved { remaining } => format!(
            "{remaining} {} still not resolved.",
            if *remaining == 1 { "file is" } else { "files are" }
        ),
        ConflictRefusal::SkipNotAvailable => "Skip is only for a rebase.".into(),
        ConflictRefusal::NotConflicted => "That file is not conflicted any more.".into(),
        ConflictRefusal::AlreadyResolved => "That file is already resolved.".into(),
    }
}

fn tag_text(problem: &TagRefusal) -> String {
    match problem {
        TagRefusal::InvalidName(_) => "That is not a valid tag name.".into(),
        TagRefusal::Message(_) => "An annotated tag needs a message.".into(),
        TagRefusal::NoCommitsYet => "There are no commits to tag yet.".into(),
        TagRefusal::AlreadyExists(name) => format!("A tag named {} already exists.", name.0),
        TagRefusal::NameConflict(name) => {
            format!("The tag {} would conflict with that name.", name.0)
        }
        TagRefusal::NoSuchTag(name) => format!("There is no tag named {}.", name.0),
        TagRefusal::NoRemote => "No remote was chosen.".into(),
    }
}

fn branch_text(problem: &BranchRefusal) -> String {
    match problem {
        BranchRefusal::InvalidName(_) => "That is not a valid branch name.".into(),
        BranchRefusal::NoCommitsYet => "This branch has no commits yet.".into(),
        BranchRefusal::AlreadyExists(name) => format!("A branch named {} already exists.", name.0),
        BranchRefusal::NameConflict(name) => {
            format!("{} conflicts with an existing branch name.", name.0)
        }
        BranchRefusal::NoSuchBranch(name) => format!("There is no branch named {}.", name.0),
        BranchRefusal::IsCurrent(name) => {
            format!("{} is the branch that is checked out.", name.0)
        }
        BranchRefusal::SameName => "The new name is the same as the old one.".into(),
    }
}

fn commit_text(problem: &CommitRefusal) -> String {
    match problem {
        CommitRefusal::Message(_) => "The commit message cannot be empty.".into(),
        CommitRefusal::NothingStaged => "Nothing is staged. Stage changes to commit.".into(),
        CommitRefusal::AmendUnborn => "There is no commit to amend yet.".into(),
        CommitRefusal::AmendDuringMerge => {
            "A merge is in progress; commit to conclude it instead of amending.".into()
        }
        CommitRefusal::UnresolvedConflicts => {
            "Some files are still conflicted. Resolve them first.".into()
        }
    }
}

/// Words for the refusals git itself gives that the user can act on, found
/// in its output. `None` for everything else (the generic words and the
/// details toggle stay).
fn known_refusal(details: &str) -> Option<&'static str> {
    const KNOWN: [(&str, &str); 3] = [
        (
            "No local changes to save",
            "Nothing was stashed: there are no changes to tracked files. New files are saved only when \"Include untracked files\" is ticked.",
        ),
        (
            "Your local changes to the following files would be overwritten",
            "Some of your local changes are in the way. Commit or stash them, then try again.",
        ),
        ("No stash entries found", "There is no stash to apply."),
    ];
    KNOWN
        .iter()
        .find(|(phrase, _)| details.contains(phrase))
        .map(|(_, words)| *words)
}

fn failure_text(failure: &Failure) -> (Tone, String) {
    let (tone, text) = match failure.class {
        ErrorClass::Expected => (Tone::Warning, "Git declined the operation."),
        ErrorClass::PreconditionChanged => (
            Tone::Warning,
            "The repository changed, or another git process is running. It has been read again; try again.",
        ),
        ErrorClass::Auth => (Tone::Error, "Authentication failed."),
        ErrorClass::Environment => (
            Tone::Error,
            "Git could not run. Check that git is installed.",
        ),
        ErrorClass::Network => (Tone::Error, "The network operation failed."),
        ErrorClass::Rejected => (Tone::Error, "Git refused, with its own message."),
        ErrorClass::Bug => (
            Tone::Error,
            "Something unexpected happened. The operation was stopped and the state read again.",
        ),
    };
    (tone, text.into())
}

/// The one line of git's output worth showing beside the words: git ends
/// with the line that says what went wrong (`fatal: Authentication
/// failed`), after the progress and the `Cloning into` lines.
pub fn headline(details: &str) -> String {
    details
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// The words for a failed operation: the class's own, or the sentence for a
/// refusal git gave that the user can act on; the raw output is the details.
pub fn failure_message(failure: &Failure) -> Message {
    let (tone, mut text) = failure_text(failure);
    if failure.class == ErrorClass::Expected
        && let Some(words) = known_refusal(&failure.details)
    {
        text = words.into();
    }
    Message {
        tone,
        text,
        details: (!failure.details.is_empty()).then(|| failure.details.clone()),
    }
}

/// The message for the newest notice, or `None` when it needs no words (an
/// operation that succeeded).
pub fn message(notice: &Notice) -> Option<Message> {
    match notice {
        Notice::Refused { action, reason } => Some(Message {
            tone: Tone::Warning,
            text: format!("{} refused: {}", action_name(*action), refusal_text(reason)),
            details: None,
        }),
        Notice::OpEnded { outcome, .. } => match outcome {
            OpOutcome::Succeeded => None,
            OpOutcome::Cancelled => Some(Message {
                tone: Tone::Warning,
                text: "Cancelled.".into(),
                details: None,
            }),
            OpOutcome::Failed(failure) => Some(failure_message(failure)),
        },
        Notice::ReadFailed { failure, .. } => Some(Message {
            tone: Tone::Error,
            text: "The repository could not be read.".into(),
            details: Some(failure.details.clone()),
        }),
        Notice::PushRejected { failure, .. } => Some(Message {
            tone: Tone::Warning,
            text: "Push rejected: the remote has new commits. Fetch and rebase to continue.".into(),
            details: Some(failure.details.clone()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use invoke_core::OpId;

    use super::*;

    #[test]
    fn a_stale_diff_says_what_to_do() {
        let message = message(&Notice::Refused {
            action: Action::Stage,
            reason: Refusal::StaleDiff,
        })
        .unwrap();
        assert_eq!(message.tone, Tone::Warning);
        assert!(message.text.starts_with("Stage refused"));
        assert!(message.text.contains("select again"));
    }

    #[test]
    fn a_success_needs_no_words() {
        assert!(
            message(&Notice::OpEnded {
                op: OpId(1),
                outcome: OpOutcome::Succeeded
            })
            .is_none()
        );
    }

    #[test]
    fn raw_git_output_is_only_in_the_details() {
        let failure = Failure {
            class: ErrorClass::Bug,
            details: "fatal: something".into(),
        };
        let message = message(&Notice::OpEnded {
            op: OpId(2),
            outcome: OpOutcome::Failed(failure),
        })
        .unwrap();
        assert!(!message.text.contains("fatal"));
        assert_eq!(message.details.as_deref(), Some("fatal: something"));
        assert_eq!(message.tone, Tone::Error);
    }

    #[test]
    fn git_refusals_the_user_can_act_on_have_their_own_words() {
        let failed = |details: &str| {
            message(&Notice::OpEnded {
                op: OpId(3),
                outcome: OpOutcome::Failed(Failure {
                    class: ErrorClass::Expected,
                    details: details.into(),
                }),
            })
            .unwrap()
        };
        let none = failed("No local changes to save\n");
        assert!(
            none.text.starts_with("Nothing was stashed"),
            "{}",
            none.text
        );
        assert_eq!(none.details.as_deref(), Some("No local changes to save\n"));
        let in_the_way = failed(
            "error: Your local changes to the following files would be overwritten by merge:\n\ta.txt",
        );
        assert!(
            in_the_way.text.contains("in the way"),
            "{}",
            in_the_way.text
        );
        assert_eq!(
            failed("No stash entries found.").text,
            "There is no stash to apply."
        );
        // Anything else keeps the generic words.
        assert_eq!(failed("fatal: odd").text, "Git declined the operation.");
        // Only an expected outcome is reworded.
        let bug = message(&Notice::OpEnded {
            op: OpId(4),
            outcome: OpOutcome::Failed(Failure {
                class: ErrorClass::Bug,
                details: "No local changes to save".into(),
            }),
        })
        .unwrap();
        assert!(!bug.text.starts_with("Nothing was stashed"));
    }

    #[test]
    fn the_drop_question_says_whether_undo_can_help() {
        assert!(crate::branch_panel::drop_text("wip", true).contains("You can undo this."));
        assert!(crate::branch_panel::drop_text("wip", false).contains("cannot be undone"));
        assert!(crate::branch_panel::drop_text("On main: wip", true).contains("On main: wip"));
    }

    #[test]
    fn the_stash_hint_follows_the_choices() {
        use crate::branch_panel::stash_hint;
        assert!(stash_hint(false, true).contains("no local changes"));
        assert!(!stash_hint(true, true).contains("stay"));
        assert!(stash_hint(true, true).contains("new files"));
        assert!(stash_hint(true, false).contains("new files stay"));
    }

    #[test]
    fn the_headline_is_the_last_line_git_printed() {
        assert_eq!(
            headline("Cloning into 'x'...\nremote: no\nfatal: Authentication failed\n\n"),
            "fatal: Authentication failed"
        );
        assert_eq!(headline(""), "");
        assert_eq!(headline("only line"), "only line");
    }
    #[test]
    fn every_error_class_has_words() {
        for class in [
            ErrorClass::Expected,
            ErrorClass::PreconditionChanged,
            ErrorClass::Auth,
            ErrorClass::Environment,
            ErrorClass::Network,
            ErrorClass::Rejected,
            ErrorClass::Bug,
        ] {
            let (_, text) = failure_text(&Failure {
                class,
                details: String::new(),
            });
            assert!(!text.is_empty());
        }
    }
}

#[cfg(test)]
mod conflict_tests {
    use super::*;

    #[test]
    fn a_refused_conflict_request_says_what_is_missing() {
        let text = |problem| {
            message(&Notice::Refused {
                action: Action::Continue,
                reason: Refusal::Conflict(problem),
            })
            .unwrap()
            .text
        };
        assert_eq!(
            text(ConflictRefusal::Unresolved { remaining: 1 }),
            "Continue refused: 1 file is still not resolved."
        );
        assert_eq!(
            text(ConflictRefusal::Unresolved { remaining: 3 }),
            "Continue refused: 3 files are still not resolved."
        );
        assert!(text(ConflictRefusal::PhaseChanged).contains("moved on"));
        for problem in [
            ConflictRefusal::NotInProgress,
            ConflictRefusal::SkipNotAvailable,
            ConflictRefusal::NotConflicted,
            ConflictRefusal::AlreadyResolved,
        ] {
            assert!(!text(problem).is_empty());
        }
    }
}
