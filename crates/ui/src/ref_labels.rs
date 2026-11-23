//! The branch, remote and tag labels shown before a commit's subject.
//! Pure: refs in, labels out.

use std::collections::HashMap;

use invoke_core::{Head, RefsSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LabelKind {
    /// The checked-out branch (drawn bold, in the accent colour).
    Current,
    Local,
    Remote,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefLabel {
    pub name: String,
    pub kind: LabelKind,
}

/// Labels by commit id (hex). For each commit: the current branch first,
/// then local branches, remote branches and tags, each group by name.
/// `refs/remotes/<remote>/HEAD` is left out (it only repeats a branch).
pub fn ref_labels(refs: &RefsSnapshot, head: &Head) -> HashMap<String, Vec<RefLabel>> {
    let current = match head {
        Head::Branch { name, .. } => Some(name.to_ref()),
        _ => None,
    };
    let mut labels: HashMap<String, Vec<RefLabel>> = HashMap::new();
    for (name, oid) in &refs.refs {
        let full = name.0.as_str();
        let label = if let Some(branch) = full.strip_prefix("refs/heads/") {
            let kind = if current.as_ref() == Some(name) {
                LabelKind::Current
            } else {
                LabelKind::Local
            };
            RefLabel {
                name: branch.to_owned(),
                kind,
            }
        } else if let Some(remote) = full.strip_prefix("refs/remotes/") {
            if remote.ends_with("/HEAD") {
                continue;
            }
            RefLabel {
                name: remote.to_owned(),
                kind: LabelKind::Remote,
            }
        } else {
            continue;
        };
        labels
            .entry(oid.as_hex().to_owned())
            .or_default()
            .push(label);
    }
    for tag in refs.tags() {
        labels
            .entry(tag.target.to_string())
            .or_default()
            .push(RefLabel {
                name: tag.name.0,
                kind: LabelKind::Tag,
            });
    }
    for list in labels.values_mut() {
        list.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    }
    labels
}

#[cfg(test)]
mod tests {
    use invoke_core::{BranchName, Oid, RefName};

    use super::*;

    fn oid(c: char) -> Oid {
        Oid::from_hex(&c.to_string().repeat(40)).unwrap()
    }

    fn refs(entries: &[(&str, char)]) -> RefsSnapshot {
        let mut snapshot = RefsSnapshot::default();
        for (name, c) in entries {
            snapshot.refs.insert(RefName((*name).to_owned()), oid(*c));
        }
        snapshot
    }

    #[test]
    fn labels_group_by_commit_and_current_comes_first() {
        let snapshot = refs(&[
            ("refs/heads/main", 'a'),
            ("refs/heads/feature", 'a'),
            ("refs/remotes/origin/main", 'a'),
            ("refs/tags/v1", 'a'),
            ("refs/heads/other", 'b'),
        ]);
        let head = Head::Branch {
            name: BranchName("main".into()),
            oid: oid('a'),
        };
        let labels = ref_labels(&snapshot, &head);
        let on_a = &labels[&"a".repeat(40)];
        let kinds: Vec<_> = on_a.iter().map(|l| (l.kind, l.name.as_str())).collect();
        assert_eq!(
            kinds,
            vec![
                (LabelKind::Current, "main"),
                (LabelKind::Local, "feature"),
                (LabelKind::Remote, "origin/main"),
                (LabelKind::Tag, "v1"),
            ]
        );
        assert_eq!(labels[&"b".repeat(40)].len(), 1);
    }

    #[test]
    fn the_remote_head_alias_is_left_out() {
        let snapshot = refs(&[("refs/remotes/origin/HEAD", 'a'), ("refs/stash", 'b')]);
        let head = Head::Detached(oid('a'));
        assert!(ref_labels(&snapshot, &head).is_empty());
    }

    #[test]
    fn an_annotated_tag_labels_its_commit() {
        let mut snapshot = refs(&[("refs/tags/v1", 'a')]);
        snapshot
            .peeled_tags
            .insert(invoke_core::TagName("v1".into()), oid('b'));
        let labels = ref_labels(&snapshot, &Head::Detached(oid('b')));
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[oid('b').as_hex()][0].name, "v1");
    }

    #[test]
    fn a_detached_head_has_no_current_branch() {
        let snapshot = refs(&[("refs/heads/main", 'a')]);
        let labels = ref_labels(&snapshot, &Head::Detached(oid('a')));
        assert_eq!(labels[&"a".repeat(40)][0].kind, LabelKind::Local);
    }
}
