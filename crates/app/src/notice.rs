//! What the user is told about the newest event: the core's `Notice` as
//! plain words. Pure. Errors name the cause and the fix; a raw git message
//! only appears as "Details" (README, "Content fundamentals").

use invoke_core::{
    Action, BranchRefusal, CommitRefusal, ErrorClass, Failure, Notice, OpOutcome, Refusal,
    UndoMoved,
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
        Refusal::RepoBusy => {
            "A merge, rebase, cherry-pick or revert is in progress. Finish or abort it first."
                .into()
        }
        Refusal::StashGone(_) => "That stash entry is gone. The list has been refreshed.".into(),
        Refusal::Branch(problem) => branch_text(problem),
        Refusal::NotRead => "The repository has not been read yet. Try again in a moment.".into(),
        Refusal::NoBranch => "This needs a checked-out branch, and HEAD is detached.".into(),
        Refusal::NoRemoteValue(_) => {
            "Force push needs the remote value you reviewed. Fetch first.".into()
        }
        Refusal::BranchChanged => "The checked-out branch changed. Try again.".into(),
        Refusal::Commit(problem) => commit_text(problem),
        Refusal::Conflict(_) => "That cannot run on the operation in progress.".into(),
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
            OpOutcome::Failed(failure) => {
                let (tone, text) = failure_text(failure);
                Some(Message {
                    tone,
                    text,
                    details: (!failure.details.is_empty()).then(|| failure.details.clone()),
                })
            }
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
