//! P3: tags in the core. Names follow `git check-ref-format`; requests are
//! checked against the fresh snapshot; local tag writes carry an undo point
//! that restores exactly their ref (law 8) and expect the ref's value on
//! disk (rule 5); pushing or deleting on a remote is irreversible (law 9).

use std::collections::BTreeMap;

use invoke_core::*;
use proptest::collection::btree_set;
use proptest::prelude::*;

const POOL: [&str; 5] = ["v1", "v2", "rel", "rel/x", "beta"];

fn oid(n: u64) -> Oid {
    Oid::from_hex(&format!("{n:040x}")).unwrap()
}

fn tag(n: &str) -> TagName {
    TagName(n.into())
}

fn disk(tags: &[&str], phase: Phase) -> DiskState {
    let mut refs = BTreeMap::new();
    refs.insert(BranchName("main".into()).to_ref(), oid(1));
    for (i, t) in tags.iter().enumerate() {
        refs.insert(tag(t).to_ref(), oid(50 + i as u64));
    }
    DiskState {
        head: Head::Branch {
            name: BranchName("main".into()),
            oid: oid(1),
        },
        phase,
        index: IndexId::Checksum(oid(100)),
        status: StatusSnapshot::default(),
        refs: RefsSnapshot {
            refs,
            ..RefsSnapshot::default()
        },
        stashes: Vec::new(),
        lock: LockState::Free,
    }
}

fn opened(disk: DiskState) -> RepoState {
    let (state, commands) = open(RepoId {
        root_commit: None,
        remote_url: None,
    });
    let Command::ReadState { read, .. } = commands[0] else {
        panic!("open must read first");
    };
    step(state, Event::StateRead { read, disk }).0
}

fn refusal(state: &RepoState) -> Option<(Action, Refusal)> {
    match &state.notice {
        Some(Notice::Refused { action, reason }) => Some((*action, reason.clone())),
        _ => None,
    }
}

fn point(commands: &[Command]) -> Option<UndoPoint> {
    commands.iter().find_map(|c| match c {
        Command::SnapshotForUndo(p) => Some(p.clone()),
        _ => None,
    })
}

fn run(commands: &[Command]) -> Option<(GitArgs, Expect)> {
    commands.iter().find_map(|c| match c {
        Command::RunGit { args, expect, .. } => Some((args.clone(), expect.clone())),
        _ => None,
    })
}

fn conflicts(existing: &str, new: &str) -> bool {
    let folder = |a: &str, b: &str| b.strip_prefix(a).is_some_and(|r| r.starts_with('/'));
    folder(existing, new) || folder(new, existing)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Creating: refused exactly when the name exists or collides as a
    /// folder; otherwise the run expects the ref to be absent and the undo
    /// point removes it again.
    #[test]
    fn create_is_checked_and_undoable(
        existing in btree_set(0..POOL.len(), 0..4),
        pick in 0..POOL.len(),
        annotated in any::<bool>(),
    ) {
        let have: Vec<&str> = existing.iter().map(|i| POOL[*i]).collect();
        let new = POOL[pick];
        let state = opened(disk(&have, Phase::Clean));
        let (after, commands) = step(state, Event::TagCreateRequested {
            name: new.into(),
            at: BranchStart::Head,
            message: annotated.then(|| "release notes".to_owned()),
        });
        if have.contains(&new) {
            prop_assert_eq!(refusal(&after), Some((
                Action::CreateTag,
                Refusal::Tag(TagRefusal::AlreadyExists(tag(new))),
            )));
            prop_assert!(commands.iter().all(|c| !c.is_write()));
        } else if let Some(other) = have.iter().find(|t| conflicts(t, new)) {
            prop_assert_eq!(refusal(&after), Some((
                Action::CreateTag,
                Refusal::Tag(TagRefusal::NameConflict(tag(other))),
            )));
            prop_assert!(commands.iter().all(|c| !c.is_write()));
        } else {
            prop_assert_eq!(refusal(&after), None);
            let (args, _) = run(&commands).unwrap();
            prop_assert_eq!(args.expected_refs(), vec![RefValue { name: tag(new).to_ref(), oid: None }]);
            let GitArgs::CreateTag { message, at, .. } = args else { panic!("not a create") };
            prop_assert_eq!(message.is_some(), annotated);
            prop_assert_eq!(at, oid(1));
            let p = point(&commands).unwrap();
            prop_assert_eq!(p.refs, vec![RefValue { name: tag(new).to_ref(), oid: None }]);
            prop_assert_eq!(p.reverse, Reversal::Refs);
            prop_assert_eq!(p.index_effect, IndexEffect::Untouched);
        }
    }

    /// Deleting a local tag: it must exist, the run expects its value, and
    /// the undo point puts that value back.
    #[test]
    fn delete_expects_and_restores_the_tag(
        existing in btree_set(0..POOL.len(), 0..4),
        pick in 0..POOL.len(),
    ) {
        let have: Vec<&str> = existing.iter().map(|i| POOL[*i]).collect();
        let name = POOL[pick];
        let state = opened(disk(&have, Phase::Clean));
        let (after, commands) = step(state, Event::TagDeleteRequested(tag(name)));
        if let Some(i) = have.iter().position(|t| *t == name) {
            let tip = oid(50 + i as u64);
            let want = vec![RefValue { name: tag(name).to_ref(), oid: Some(tip.clone()) }];
            let (args, _) = run(&commands).unwrap();
            prop_assert_eq!(args.expected_refs(), want.clone());
            prop_assert_eq!(point(&commands).unwrap().refs, want);
        } else {
            prop_assert_eq!(refusal(&after), Some((
                Action::DeleteTag,
                Refusal::Tag(TagRefusal::NoSuchTag(tag(name))),
            )));
            prop_assert!(commands.iter().all(|c| !c.is_write()));
        }
    }

    /// Pushing a tag or deleting one on a remote leaves an irreversible
    /// marker and never restores refs (law 9); the push needs the local tag.
    #[test]
    fn remote_tag_writes_are_irreversible(
        exists in any::<bool>(),
        delete in any::<bool>(),
    ) {
        let have: &[&str] = if exists { &["v1"] } else { &[] };
        let state = opened(disk(have, Phase::Clean));
        let event = if delete {
            Event::RemoteTagDeleteRequested { remote: "origin".into(), name: tag("v1") }
        } else {
            Event::TagPushRequested { remote: "origin".into(), name: tag("v1") }
        };
        let (after, commands) = step(state, event);
        if exists || delete {
            let p = point(&commands).unwrap();
            prop_assert_eq!(p.reverse, Reversal::Never(Irreversible::Push));
            let (args, _) = run(&commands).unwrap();
            prop_assert!(args.is_network());
        } else {
            prop_assert_eq!(refusal(&after), Some((
                Action::PushTag,
                Refusal::Tag(TagRefusal::NoSuchTag(tag("v1"))),
            )));
        }
    }

    /// While a merge (or other operation) is paused, no tag request runs.
    #[test]
    fn a_paused_operation_blocks_tags(which in 0..4usize) {
        let phase = Phase::Merging { conflicts: Conflicts::default() };
        let state = opened(disk(&["v1"], phase));
        let event = match which {
            0 => Event::TagCreateRequested { name: "new".into(), at: BranchStart::Head, message: None },
            1 => Event::TagDeleteRequested(tag("v1")),
            2 => Event::TagPushRequested { remote: "origin".into(), name: tag("v1") },
            _ => Event::RemoteTagDeleteRequested { remote: "origin".into(), name: tag("v1") },
        };
        let (after, commands) = step(state, event);
        prop_assert!(matches!(refusal(&after), Some((_, Refusal::RepoBusy))));
        prop_assert!(commands.iter().all(|c| !c.is_write()));
    }

    /// Names typed by the user are parsed at the boundary: a bad name or a
    /// blank annotation message is refused before anything is queued.
    #[test]
    fn bad_input_is_refused_at_the_boundary(
        bad in proptest::sample::select(vec!["", "-x", "a b", "a..b", "x.lock", "HEAD"]),
    ) {
        let state = opened(disk(&[], Phase::Clean));
        let (after, _) = step(state.clone(), Event::TagCreateRequested {
            name: bad.into(), at: BranchStart::Head, message: None,
        });
        prop_assert!(matches!(refusal(&after), Some((Action::CreateTag, Refusal::Tag(TagRefusal::InvalidName(_))))));
        prop_assert!(after.ops.queued.is_empty());
        let (after, _) = step(state, Event::TagCreateRequested {
            name: "ok".into(), at: BranchStart::Head, message: Some("  \n".into()),
        });
        prop_assert!(matches!(refusal(&after), Some((Action::CreateTag, Refusal::Tag(TagRefusal::Message(_))))));
    }
}

/// Tags come from the refs read from disk; an annotated tag is badged on
/// the commit it peels to.
#[test]
fn tags_are_derived_from_the_refs() {
    let mut d = disk(&["v1", "v2"], Phase::Clean);
    d.refs.peeled_tags.insert(tag("v2"), oid(7));
    let tags = d.refs.tags();
    assert_eq!(tags.len(), 2);
    assert!(!tags[0].annotated && tags[0].target == tags[0].tip);
    assert!(tags[1].annotated && tags[1].target == oid(7) && tags[1].tip == oid(51));
}
