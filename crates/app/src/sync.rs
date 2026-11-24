//! What fetch, pull and push should be asked to do for the repo on screen,
//! and the words for an operation in progress. Pure: state in, requests and
//! text out. The buttons and the strip that draw them are in `sync_view`.
//!
//! The remote branch comes from the branch's upstream. A branch without one
//! is published to the remote it would most likely belong to (`origin`, else
//! the remote already in use), under its own name, and the push sets the
//! upstream so the next one needs no question.

use invoke_core::{
    BranchName, Event, Head, LockState, Phase, Progress, PullMode, RefName, RefsSnapshot,
    RemoteBranch, RepoState, RunningOp, Snapshot, Tracking, WriteRequest,
};

/// What the buttons act on, worked out from one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The checked-out branch; none when HEAD is detached or unborn.
    pub local: Option<BranchName>,
    /// The remote that fetch asks and that a first push publishes to.
    pub remote: String,
    pub upstream: Option<RemoteBranch>,
    pub tracking: Option<Tracking>,
    /// No merge, rebase, cherry-pick or revert is in progress.
    pub clean: bool,
}

/// `refs/remotes/origin/feature/x` is the branch `feature/x` on `origin`: a
/// remote name never holds a slash in practice, so the first one splits.
pub fn split_tracking_ref(name: &RefName) -> Option<RemoteBranch> {
    let rest = name.0.strip_prefix("refs/remotes/")?;
    let (remote, branch) = rest.split_once('/')?;
    if remote.is_empty() || branch.is_empty() || branch == "HEAD" {
        return None;
    }
    Some(RemoteBranch {
        remote: remote.to_owned(),
        branch: BranchName(branch.to_owned()),
    })
}

/// The remote to ask when the branch has no upstream: `origin` if we know
/// it or know no other, else the first one we have refs of, else `origin`
/// (a remote that was added but never fetched leaves no refs behind).
fn default_remote(refs: &RefsSnapshot) -> String {
    let mut remotes: Vec<String> = refs
        .refs
        .keys()
        .filter_map(split_tracking_ref)
        .map(|branch| branch.remote)
        .collect();
    remotes.sort();
    remotes.dedup();
    if remotes.is_empty() || remotes.iter().any(|remote| remote == "origin") {
        "origin".into()
    } else {
        remotes.swap_remove(0)
    }
}

/// `None` until the repo has been read.
pub fn plan(state: &RepoState) -> Option<Plan> {
    let Snapshot::Read { disk, .. } = &state.snapshot else {
        return None;
    };
    let local = match &disk.head {
        Head::Branch { name, .. } => Some(name.clone()),
        Head::Detached(_) | Head::Unborn(_) => None,
    };
    let configured = match &disk.head {
        Head::Branch { name, .. } | Head::Unborn(name) => disk.refs.upstreams.get(name),
        Head::Detached(_) => None,
    };
    let upstream = configured.and_then(|upstream| split_tracking_ref(&upstream.name));
    let remote = upstream
        .as_ref()
        .map(|upstream| upstream.remote.clone())
        .unwrap_or_else(|| default_remote(&disk.refs));
    Some(Plan {
        local,
        remote,
        upstream,
        tracking: configured.map(|upstream| upstream.tracking),
        clean: matches!(disk.phase, Phase::Clean),
    })
}

impl Plan {
    pub fn ahead(&self) -> u32 {
        match self.tracking {
            Some(Tracking::Apart { ahead, .. }) => ahead,
            _ => 0,
        }
    }

    pub fn behind(&self) -> u32 {
        match self.tracking {
            Some(Tracking::Apart { behind, .. }) => behind,
            _ => 0,
        }
    }

    pub fn fetch(&self) -> Event {
        Event::FetchRequested {
            remote: self.remote.clone(),
        }
    }

    /// Only with an upstream that exists and with nothing in progress that
    /// a pull would run into (the core refuses it too).
    pub fn can_pull(&self) -> bool {
        self.clean
            && self.local.is_some()
            && self.upstream.is_some()
            && !matches!(self.tracking, Some(Tracking::Gone) | None)
    }

    pub fn pull(&self, mode: PullMode) -> Option<Event> {
        if !self.can_pull() {
            return None;
        }
        Some(Event::PullRequested {
            from: self.upstream.clone()?,
            mode,
        })
    }

    /// Both a merge and a rebase make sense only when the two sides have
    /// commits the other lacks.
    pub fn diverged(&self) -> bool {
        self.ahead() > 0 && self.behind() > 0
    }

    /// A branch without an upstream is published; one that has commits the
    /// remote lacks is pushed; one that is level has nothing to push.
    pub fn can_push(&self) -> bool {
        self.local.is_some()
            && match self.tracking {
                None | Some(Tracking::Gone) => true,
                Some(Tracking::Apart { ahead, .. }) => ahead > 0,
            }
    }

    /// The first push of a branch, which also sets its upstream.
    pub fn publishes(&self) -> bool {
        self.upstream.is_none()
    }

    /// Where a push goes: the upstream, or the same name on the default
    /// remote.
    pub fn push_target(&self) -> Option<RemoteBranch> {
        self.upstream.clone().or_else(|| {
            Some(RemoteBranch {
                remote: self.remote.clone(),
                branch: self.local.clone()?,
            })
        })
    }

    /// `force` leases against the remote value in the snapshot (the core
    /// takes it, and refuses when there is none).
    pub fn push(&self, force: bool) -> Option<Event> {
        if !self.can_push() {
            return None;
        }
        Some(Event::PushRequested {
            to: self.push_target()?,
            force_with_lease: force,
            set_upstream: self.publishes(),
        })
    }
}

/// Another process holds `index.lock` while none of our operations runs: it
/// may be a leftover that the user can choose to remove.
pub fn lock_to_offer(state: &RepoState) -> bool {
    matches!(
        &state.snapshot,
        Snapshot::Read { disk, .. } if disk.lock == LockState::HeldByOther
    ) && state.ops.running.is_none()
        && state.ops.queued.is_empty()
}

/// `origin/main`, for sentences.
pub fn remote_text(target: &RemoteBranch) -> String {
    format!("{}/{}", target.remote, target.branch.0)
}

/// What a network operation is doing, in words; `None` for the writes that
/// are quick enough not to need a strip.
pub fn what(request: &WriteRequest) -> Option<String> {
    match request {
        WriteRequest::Fetch { remote } => Some(format!("Fetching {remote}")),
        WriteRequest::Pull { from, mode, .. } => Some(match mode {
            PullMode::Merge => format!("Pulling {}", remote_text(from)),
            PullMode::Rebase => format!("Pulling {} with rebase", remote_text(from)),
        }),
        WriteRequest::Push { to, lease, .. } => Some(if lease.is_some() {
            format!("Force pushing to {}", remote_text(to))
        } else {
            format!("Pushing to {}", remote_text(to))
        }),
        WriteRequest::WorktreeAdd { path, .. } => Some(format!("Adding worktree {path}")),
        WriteRequest::WorktreeRemove { path, .. } => Some(format!("Removing worktree {path}")),
        WriteRequest::Submodule { op, path } => {
            let verb = match op {
                invoke_core::SubmoduleOp::Init => "Initializing",
                invoke_core::SubmoduleOp::UpdateInitRecursive => "Updating",
                invoke_core::SubmoduleOp::SyncRecursive => "Syncing",
            };
            Some(match path {
                Some(path) => format!("{verb} submodule {}", path.0),
                None => format!("{verb} submodules"),
            })
        }
        WriteRequest::LfsPull { path } => Some(match path {
            Some(path) => format!("Pulling LFS object {}", path.0),
            None => "Pulling Git LFS objects".to_owned(),
        }),
        _ => None,
    }
}

/// The stage git printed, and how far it is when it said a total.
pub fn progress(running: &RunningOp) -> (String, Option<f32>) {
    match &running.phase {
        invoke_core::OpPhase::Running {
            progress: Some(Progress { stage, done, total }),
        } => (
            stage.clone(),
            total
                .filter(|total| *total > 0)
                .map(|total| (*done as f32 / total as f32).clamp(0.0, 1.0)),
        ),
        _ => ("Connecting".into(), None),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use invoke_core::{Lease, Oid, RepoId, Upstream};
    use proptest::prelude::*;

    use super::*;

    fn branch(name: &str) -> BranchName {
        BranchName(name.into())
    }

    fn plan_with(
        local: Option<&str>,
        upstream: Option<(&str, Tracking)>,
        extra_refs: &[&str],
        clean: bool,
    ) -> Plan {
        let mut refs = RefsSnapshot::default();
        let oid = Oid::from_hex(&"a".repeat(40)).unwrap();
        for name in extra_refs {
            refs.refs.insert(RefName((*name).into()), oid.clone());
        }
        let mut upstreams = BTreeMap::new();
        if let (Some(local), Some((name, tracking))) = (local, upstream) {
            upstreams.insert(
                branch(local),
                Upstream {
                    name: RefName(name.into()),
                    tracking,
                },
            );
        }
        refs.upstreams = upstreams;
        let upstream_ref = refs
            .upstreams
            .values()
            .next()
            .and_then(|up| split_tracking_ref(&up.name));
        let tracking = refs.upstreams.values().next().map(|up| up.tracking);
        Plan {
            local: local.map(branch),
            remote: upstream_ref
                .as_ref()
                .map(|up| up.remote.clone())
                .unwrap_or_else(|| default_remote(&refs)),
            upstream: upstream_ref,
            tracking,
            clean,
        }
    }

    fn apart(ahead: u32, behind: u32) -> Tracking {
        Tracking::Apart { ahead, behind }
    }

    #[test]
    fn a_tracking_ref_names_its_remote_and_branch() {
        let name = |text: &str| split_tracking_ref(&RefName(text.into()));
        assert_eq!(
            name("refs/remotes/origin/main"),
            Some(RemoteBranch {
                remote: "origin".into(),
                branch: branch("main")
            })
        );
        assert_eq!(
            name("refs/remotes/up/feature/x").unwrap().branch,
            branch("feature/x")
        );
        assert_eq!(name("refs/remotes/origin/HEAD"), None);
        assert_eq!(name("refs/heads/main"), None);
        assert_eq!(name("refs/remotes/origin"), None);
    }

    #[test]
    fn a_branch_with_an_upstream_pushes_and_pulls_there() {
        let plan = plan_with(
            Some("main"),
            Some(("refs/remotes/upstream/main", apart(2, 1))),
            &[],
            true,
        );
        assert_eq!(plan.remote, "upstream");
        assert!(plan.diverged());
        let Some(Event::PushRequested {
            to,
            force_with_lease,
            set_upstream,
        }) = plan.push(false)
        else {
            panic!("push expected");
        };
        assert_eq!(remote_text(&to), "upstream/main");
        assert!(!force_with_lease && !set_upstream);
        let Some(Event::PullRequested { from, mode }) = plan.pull(PullMode::Rebase) else {
            panic!("pull expected");
        };
        assert_eq!(remote_text(&from), "upstream/main");
        assert_eq!(mode, PullMode::Rebase);
    }

    #[test]
    fn a_branch_without_an_upstream_is_published_and_gets_one() {
        let plan = plan_with(Some("topic"), None, &["refs/remotes/origin/main"], true);
        assert!(plan.publishes());
        let Some(Event::PushRequested {
            to, set_upstream, ..
        }) = plan.push(false)
        else {
            panic!("push expected");
        };
        assert_eq!(remote_text(&to), "origin/topic");
        assert!(set_upstream);
        assert!(plan.pull(PullMode::Merge).is_none());
    }

    #[test]
    fn the_default_remote_is_origin_or_the_one_in_use() {
        let refs = |names: &[&str]| plan_with(Some("t"), None, names, true).remote;
        assert_eq!(refs(&[]), "origin");
        assert_eq!(refs(&["refs/remotes/fork/main"]), "fork");
        assert_eq!(
            refs(&["refs/remotes/fork/main", "refs/remotes/origin/main"]),
            "origin"
        );
    }

    #[test]
    fn nothing_is_pushed_when_there_is_nothing_ahead() {
        let level = plan_with(
            Some("main"),
            Some(("refs/remotes/origin/main", apart(0, 3))),
            &[],
            true,
        );
        assert!(!level.can_push());
        assert!(level.push(false).is_none());
        assert!(level.can_pull());
    }

    #[test]
    fn a_gone_upstream_can_be_pushed_again_but_not_pulled() {
        let gone = plan_with(
            Some("main"),
            Some(("refs/remotes/origin/main", Tracking::Gone)),
            &[],
            true,
        );
        assert!(gone.can_push() && !gone.can_pull());
        let Some(Event::PushRequested { set_upstream, .. }) = gone.push(false) else {
            panic!("push expected");
        };
        assert!(!set_upstream);
    }

    #[test]
    fn a_detached_head_can_only_fetch() {
        let plan = plan_with(None, None, &[], true);
        assert!(!plan.can_push() && !plan.can_pull());
        assert!(plan.push(false).is_none() && plan.pull(PullMode::Merge).is_none());
        assert!(matches!(plan.fetch(), Event::FetchRequested { .. }));
    }

    #[test]
    fn a_pull_waits_for_a_merge_or_rebase_to_finish() {
        let plan = plan_with(
            Some("main"),
            Some(("refs/remotes/origin/main", apart(0, 1))),
            &[],
            false,
        );
        assert!(plan.pull(PullMode::Merge).is_none());
    }

    #[test]
    fn the_words_follow_the_request() {
        let to = RemoteBranch {
            remote: "origin".into(),
            branch: branch("main"),
        };
        let push = |lease| WriteRequest::Push {
            local: branch("main"),
            to: to.clone(),
            lease,
            set_upstream: false,
        };
        assert_eq!(what(&push(None)).unwrap(), "Pushing to origin/main");
        let lease = Lease {
            remote_ref: RefName("refs/heads/main".into()),
            expected: Oid::from_hex(&"b".repeat(40)).unwrap(),
        };
        assert_eq!(
            what(&push(Some(lease))).unwrap(),
            "Force pushing to origin/main"
        );
        assert_eq!(
            what(&WriteRequest::Fetch {
                remote: "origin".into()
            })
            .unwrap(),
            "Fetching origin"
        );
        assert!(what(&WriteRequest::Undo).is_none());
    }

    #[test]
    fn a_lock_is_offered_only_for_someone_elses_while_we_are_idle() {
        let id = RepoId {
            root_commit: None,
            remote_url: None,
        };
        let with = |lock| {
            let mut state = RepoState::new(id.clone());
            state.snapshot = Snapshot::Read {
                read: invoke_core::ReadId(1),
                disk: invoke_core::DiskState {
                    head: Head::Unborn(branch("main")),
                    phase: Phase::Clean,
                    index: invoke_core::IndexId::Absent,
                    status: invoke_core::StatusSnapshot::default(),
                    refs: RefsSnapshot::default(),
                    stashes: Vec::new(),
                    lock,
                    advanced: Default::default(),
                },
            };
            state
        };
        assert!(lock_to_offer(&with(LockState::HeldByOther)));
        assert!(!lock_to_offer(&with(LockState::Free)));
        assert!(!lock_to_offer(&with(LockState::HeldByUs)));
        assert!(!lock_to_offer(&RepoState::new(id.clone())));
        // One of our own operations running means the lock may be its own.
        let mut busy = with(LockState::HeldByOther);
        busy.ops.queued.push_back(invoke_core::QueuedOp {
            id: invoke_core::OpId(1),
            request: WriteRequest::Undo,
        });
        assert!(!lock_to_offer(&busy));
    }

    #[test]
    fn a_fresh_state_has_no_plan() {
        let state = RepoState::new(RepoId {
            root_commit: None,
            remote_url: None,
        });
        assert!(plan(&state).is_none());
    }

    proptest! {
        // However the tracking looks, a push or pull is never built without
        // the remote branch it names, and a force is only a flag on a push
        // the core still has to lease.
        #[test]
        fn requests_always_name_a_remote_branch(
            ahead in 0u32..5, behind in 0u32..5, gone: bool, clean: bool, has_upstream: bool,
        ) {
            let tracking = if gone { Tracking::Gone } else { apart(ahead, behind) };
            let plan = plan_with(
                Some("main"),
                has_upstream.then_some(("refs/remotes/origin/main", tracking)),
                &[],
                clean,
            );
            if let Some(Event::PushRequested { to, set_upstream, .. }) = plan.push(true) {
                prop_assert!(!to.remote.is_empty() && !to.branch.0.is_empty());
                prop_assert_eq!(set_upstream, !has_upstream);
            }
            if let Some(Event::PullRequested { from, .. }) = plan.pull(PullMode::Merge) {
                prop_assert!(has_upstream && clean && !gone);
                prop_assert!(!from.remote.is_empty());
            }
            prop_assert_eq!(plan.can_pull(), plan.pull(PullMode::Rebase).is_some());
            prop_assert_eq!(plan.can_push(), plan.push(false).is_some());
        }
    }
}
