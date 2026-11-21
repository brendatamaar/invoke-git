//! Clone jobs: a small pure state machine beside the repo core.
//!
//! A clone runs before any repository exists, so there is no `RepoState` to
//! hold it. `CloneState` is its own `step(state, event) -> (state, commands)`
//! with the same op lifecycle as `OpPhase` (running with progress, awaiting
//! auth, cancelling) and the same rule: every job ends with exactly one
//! `Finished` event, whatever happened (success, failure or cancel).
//!
//! The job ends in a phase that stays until the UI dismisses it, so a
//! failure can be shown. Once the clone succeeds the app opens the new repo
//! with `step::open`; nothing here knows about repos.

use std::path::PathBuf;

use crate::ids::OpId;
use crate::ops::{AuthAnswer, AuthPrompt, ErrorClass, Failure, OpOutcome, Progress};

/// What the user asked to clone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloneRequest {
    pub url: String,
    /// The folder to create. Must be absolute; the shell refuses otherwise.
    pub target: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClonePhase {
    Running {
        progress: Option<Progress>,
    },
    /// Waiting for the user to answer a credential prompt.
    AwaitingAuth(AuthPrompt),
    /// Cancel was sent; waiting for the shell to report the end.
    Cancelling,
    /// Over. Stays until `Dismissed` so the UI can show the outcome.
    Ended(OpOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloneJob {
    pub id: OpId,
    pub request: CloneRequest,
    pub phase: ClonePhase,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloneState {
    next: u64,
    pub jobs: Vec<CloneJob>,
}

impl CloneState {
    pub fn job(&self, id: OpId) -> Option<&CloneJob> {
        self.jobs.iter().find(|job| job.id == id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloneEvent {
    // From the UI.
    StartRequested(CloneRequest),
    CancelRequested(OpId),
    AuthAnswered(OpId, AuthAnswer),
    Dismissed(OpId),
    // From the shell.
    Progress(OpId, Progress),
    AuthNeeded(OpId, AuthPrompt),
    Finished(OpId, OpOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloneCommand {
    Start { op: OpId, request: CloneRequest },
    Cancel(OpId),
    AnswerAuth(OpId, AuthAnswer),
}

/// `step(state, event) -> (state, commands)` for clone jobs. It does no IO
/// and never panics; events for unknown or ended jobs are ignored.
pub fn step_clone(mut state: CloneState, event: CloneEvent) -> (CloneState, Vec<CloneCommand>) {
    let mut commands = Vec::new();
    match event {
        CloneEvent::StartRequested(request) => {
            let id = OpId(state.next);
            state.next = state.next.saturating_add(1);
            let phase = match validate(&request) {
                Some(failure) => ClonePhase::Ended(OpOutcome::Failed(failure)),
                None => {
                    commands.push(CloneCommand::Start {
                        op: id,
                        request: request.clone(),
                    });
                    ClonePhase::Running { progress: None }
                }
            };
            state.jobs.push(CloneJob { id, request, phase });
        }
        CloneEvent::CancelRequested(op) => {
            if let Some(job) = live_mut(&mut state, op)
                && !matches!(job.phase, ClonePhase::Cancelling)
            {
                job.phase = ClonePhase::Cancelling;
                commands.push(CloneCommand::Cancel(op));
            }
        }
        CloneEvent::AuthAnswered(op, answer) => {
            if let Some(job) = live_mut(&mut state, op)
                && matches!(job.phase, ClonePhase::AwaitingAuth(_))
            {
                job.phase = ClonePhase::Running { progress: None };
                commands.push(CloneCommand::AnswerAuth(op, answer));
            }
        }
        CloneEvent::Dismissed(op) => {
            state
                .jobs
                .retain(|job| job.id != op || !matches!(job.phase, ClonePhase::Ended(_)));
        }
        CloneEvent::Progress(op, progress) => {
            if let Some(job) = live_mut(&mut state, op)
                && let ClonePhase::Running { progress: current } = &mut job.phase
            {
                *current = Some(progress);
            }
        }
        CloneEvent::AuthNeeded(op, prompt) => {
            if let Some(job) = live_mut(&mut state, op)
                && !matches!(job.phase, ClonePhase::Cancelling)
            {
                job.phase = ClonePhase::AwaitingAuth(prompt);
            }
        }
        CloneEvent::Finished(op, outcome) => {
            if let Some(job) = live_mut(&mut state, op) {
                job.phase = ClonePhase::Ended(outcome);
            }
        }
    }
    (state, commands)
}

/// A job that has not ended yet.
fn live_mut(state: &mut CloneState, op: OpId) -> Option<&mut CloneJob> {
    state
        .jobs
        .iter_mut()
        .find(|job| job.id == op && !matches!(job.phase, ClonePhase::Ended(_)))
}

fn validate(request: &CloneRequest) -> Option<Failure> {
    let reason = if request.url.trim().is_empty() {
        "Enter the address of the repository to clone."
    } else if request.target.as_os_str().is_empty() {
        "Choose a folder to clone into."
    } else {
        return None;
    };
    Some(Failure {
        class: ErrorClass::Expected,
        details: reason.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn request(url: &str) -> CloneRequest {
        CloneRequest {
            url: url.into(),
            target: PathBuf::from("/work/repo"),
        }
    }

    fn prompt() -> AuthPrompt {
        AuthPrompt {
            kind: crate::ops::AuthKind::Password,
            text: "Password: ".into(),
        }
    }

    fn feed(events: impl IntoIterator<Item = CloneEvent>) -> (CloneState, Vec<CloneCommand>) {
        let mut state = CloneState::default();
        let mut all = Vec::new();
        for event in events {
            let (next, commands) = step_clone(state, event);
            state = next;
            all.extend(commands);
        }
        (state, all)
    }

    #[test]
    fn lifecycle_with_auth_then_success() {
        let op = OpId(0);
        let (state, commands) = feed([
            CloneEvent::StartRequested(request("https://example.com/r.git")),
            CloneEvent::Progress(
                op,
                Progress {
                    stage: "Receiving objects".into(),
                    done: 1,
                    total: Some(2),
                },
            ),
            CloneEvent::AuthNeeded(op, prompt()),
            CloneEvent::AuthAnswered(op, AuthAnswer("pw".into())),
            CloneEvent::Finished(op, OpOutcome::Succeeded),
        ]);
        assert_eq!(
            commands,
            [
                CloneCommand::Start {
                    op,
                    request: request("https://example.com/r.git")
                },
                CloneCommand::AnswerAuth(op, AuthAnswer("pw".into())),
            ]
        );
        assert_eq!(
            state.job(op).unwrap().phase,
            ClonePhase::Ended(OpOutcome::Succeeded)
        );
    }

    #[test]
    fn invalid_requests_end_at_once_without_a_command() {
        let (state, commands) = feed([CloneEvent::StartRequested(request("  "))]);
        assert!(commands.is_empty());
        assert!(matches!(
            state.jobs[0].phase,
            ClonePhase::Ended(OpOutcome::Failed(Failure {
                class: ErrorClass::Expected,
                ..
            }))
        ));
    }

    #[test]
    fn cancel_is_sent_once_and_blocks_auth() {
        let op = OpId(0);
        let (state, commands) = feed([
            CloneEvent::StartRequested(request("u")),
            CloneEvent::CancelRequested(op),
            CloneEvent::CancelRequested(op),
            CloneEvent::AuthNeeded(op, prompt()),
        ]);
        let cancels = commands
            .iter()
            .filter(|c| matches!(c, CloneCommand::Cancel(_)))
            .count();
        assert_eq!(cancels, 1);
        assert_eq!(state.jobs[0].phase, ClonePhase::Cancelling);
    }

    #[test]
    fn answers_only_flow_while_awaiting_auth() {
        let op = OpId(0);
        let (_, commands) = feed([
            CloneEvent::StartRequested(request("u")),
            CloneEvent::AuthAnswered(op, AuthAnswer("x".into())),
        ]);
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, CloneCommand::AnswerAuth(..)))
        );
    }

    #[test]
    fn dismiss_removes_only_ended_jobs() {
        let op = OpId(0);
        let (state, _) = feed([
            CloneEvent::StartRequested(request("u")),
            CloneEvent::Dismissed(op),
        ]);
        assert_eq!(state.jobs.len(), 1);
        let (state, _) = feed([
            CloneEvent::StartRequested(request("u")),
            CloneEvent::Finished(op, OpOutcome::Cancelled),
            CloneEvent::Dismissed(op),
        ]);
        assert!(state.jobs.is_empty());
    }

    fn arb_event() -> impl Strategy<Value = CloneEvent> {
        let op = (0u64..3).prop_map(OpId);
        let outcome = prop_oneof![
            Just(OpOutcome::Succeeded),
            Just(OpOutcome::Cancelled),
            Just(OpOutcome::Failed(Failure {
                class: ErrorClass::Network,
                details: String::new()
            })),
        ];
        prop_oneof![
            "[a-z ]{0,3}".prop_map(|url| CloneEvent::StartRequested(request(&url))),
            op.clone().prop_map(CloneEvent::CancelRequested),
            op.clone()
                .prop_map(|op| CloneEvent::AuthAnswered(op, AuthAnswer("s".into()))),
            op.clone().prop_map(CloneEvent::Dismissed),
            op.clone().prop_map(|op| CloneEvent::Progress(
                op,
                Progress {
                    stage: "s".into(),
                    done: 1,
                    total: None
                }
            )),
            op.clone()
                .prop_map(|op| CloneEvent::AuthNeeded(op, prompt())),
            (op, outcome).prop_map(|(op, outcome)| CloneEvent::Finished(op, outcome)),
        ]
    }

    proptest! {
        /// Law 13 shape: each job is started at most once, cancelled at most
        /// once, answered only while awaiting auth, and an ended job never
        /// changes again.
        #[test]
        fn lifecycle_invariants(events in proptest::collection::vec(arb_event(), 0..40)) {
            let mut state = CloneState::default();
            let mut started = std::collections::BTreeSet::new();
            let mut cancelled = std::collections::BTreeSet::new();
            for event in events {
                let before = state.clone();
                let (next, commands) = step_clone(state, event);
                for command in &commands {
                    match command {
                        CloneCommand::Start { op, .. } => {
                            prop_assert!(started.insert(*op));
                            prop_assert!(next.job(*op).is_some());
                        }
                        CloneCommand::Cancel(op) => {
                            prop_assert!(started.contains(op));
                            prop_assert!(cancelled.insert(*op));
                        }
                        CloneCommand::AnswerAuth(op, _) => {
                            let was = before.job(*op).map(|job| &job.phase);
                            prop_assert!(matches!(was, Some(ClonePhase::AwaitingAuth(_))));
                        }
                    }
                }
                for old in &before.jobs {
                    if matches!(old.phase, ClonePhase::Ended(_))
                        && let Some(new) = next.job(old.id)
                    {
                        prop_assert_eq!(&new.phase, &old.phase);
                    }
                }
                state = next;
            }
        }
    }
}
