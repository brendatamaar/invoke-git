//! Undo entries as one line of text each (task 22, part 2), so the stack can
//! be kept in the app's store and restored after a restart. Pure.
//!
//! The format is a list of fields separated by tabs, led by a version word
//! (`u1`). Text fields (labels, ref names, paths, stash messages) escape `\`,
//! tab, line feed and carriage return, so a field never contains a separator.
//! Counts come before the lists they count. A line that is not exactly what
//! [`encode`] writes is an error, never a guess: an entry that cannot be read
//! is dropped by the caller, and the worst that happens is one undo less.

use crate::event::{HeadTarget, IndexEffect, RefValue, UndoPoint};
use crate::ids::{BranchName, IndexId, Oid, OpId, RefName, RepoPath};
use crate::stash::StashEntry;
use crate::undo::{Irreversible, Reversal, Reversible, UndoEntry};

const VERSION: &str = "u1";

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            other => return Err(format!("bad escape {other:?}")),
        }
    }
    Ok(out)
}

fn irreversible_word(why: Irreversible) -> &'static str {
    match why {
        Irreversible::Push => "push",
        Irreversible::StashRewroteFiles => "stash-rewrote-files",
        Irreversible::StashNotOnTop => "stash-not-on-top",
        Irreversible::Pull => "pull",
        Irreversible::MergeConcluded => "merge-concluded",
        Irreversible::Continued => "continued",
    }
}

fn irreversible_of(word: &str) -> Result<Irreversible, String> {
    Ok(match word {
        "push" => Irreversible::Push,
        "stash-rewrote-files" => Irreversible::StashRewroteFiles,
        "stash-not-on-top" => Irreversible::StashNotOnTop,
        "pull" => Irreversible::Pull,
        "merge-concluded" => Irreversible::MergeConcluded,
        "continued" => Irreversible::Continued,
        other => return Err(format!("unknown reason {other:?}")),
    })
}

struct Writer(Vec<String>);

impl Writer {
    fn word(&mut self, word: impl Into<String>) {
        self.0.push(word.into());
    }

    fn text(&mut self, text: &str) {
        self.0.push(escape(text));
    }

    fn head(&mut self, head: &HeadTarget) {
        match head {
            HeadTarget::Branch(name) => {
                self.word("branch");
                self.text(&name.0);
            }
            HeadTarget::Detached => self.word("detached"),
        }
    }

    fn index(&mut self, index: &IndexId) {
        match index {
            IndexId::Absent => self.word("absent"),
            IndexId::Checksum(oid) => {
                self.word("checksum");
                self.word(oid.as_hex());
            }
        }
    }

    fn refs(&mut self, refs: &[RefValue]) {
        self.word(refs.len().to_string());
        for value in refs {
            self.text(&value.name.0);
            match &value.oid {
                Some(oid) => self.word(oid.as_hex()),
                None => self.word("-"),
            }
        }
    }

    fn stash(&mut self, entry: &StashEntry) {
        self.word(entry.id.as_hex());
        self.text(&entry.message);
    }
}

/// One undo entry as one line (no line break inside).
pub fn encode(entry: &UndoEntry) -> String {
    let mut w = Writer(Vec::new());
    w.word(VERSION);
    let point = &entry.point;
    w.word(point.op.0.to_string());
    w.text(&point.label);
    w.head(&point.head);
    w.refs(&point.refs);
    w.index(&point.index);
    w.word(match point.index_effect {
        IndexEffect::Rewrites => "rewrites",
        IndexEffect::Untouched => "untouched",
    });
    match &point.reverse {
        Reversal::Refs => w.word("refs"),
        Reversal::StashStore(stash) => {
            w.word("stash-store");
            w.stash(stash);
        }
        Reversal::StashKept(stash) => {
            w.word("stash-kept");
            w.stash(stash);
        }
        Reversal::Never(why) => {
            w.word("never");
            w.word(irreversible_word(*why));
        }
        Reversal::Discard { path } => {
            w.word("discard");
            w.text(&path.0);
        }
    }
    w.word(entry.tree.as_hex());
    w.refs(&entry.after_refs);
    w.index(&entry.after_index);
    w.head(&entry.after_head);
    match entry.reversible {
        Reversible::Yes => w.word("yes"),
        Reversible::No(why) => {
            w.word("no");
            w.word(irreversible_word(why));
        }
    }
    w.0.join("\t")
}

struct Reader<'a>(std::str::Split<'a, char>);

impl<'a> Reader<'a> {
    fn next(&mut self, what: &str) -> Result<&'a str, String> {
        self.0.next().ok_or_else(|| format!("missing {what}"))
    }

    fn text(&mut self, what: &str) -> Result<String, String> {
        unescape(self.next(what)?)
    }

    fn oid(&mut self, what: &str) -> Result<Oid, String> {
        Oid::from_hex(self.next(what)?).map_err(|e| format!("bad {what}: {e:?}"))
    }

    fn count(&mut self, what: &str) -> Result<usize, String> {
        let field = self.next(what)?;
        let count: usize = field.parse().map_err(|_| format!("bad {what}"))?;
        // One spelling per number, so a line has one meaning.
        (count.to_string() == field)
            .then_some(count)
            .ok_or_else(|| format!("bad {what}"))
    }

    fn head(&mut self) -> Result<HeadTarget, String> {
        match self.next("head")? {
            "branch" => Ok(HeadTarget::Branch(BranchName(self.text("branch")?))),
            "detached" => Ok(HeadTarget::Detached),
            other => Err(format!("unknown head {other:?}")),
        }
    }

    fn index(&mut self) -> Result<IndexId, String> {
        match self.next("index")? {
            "absent" => Ok(IndexId::Absent),
            "checksum" => Ok(IndexId::Checksum(self.oid("index checksum")?)),
            other => Err(format!("unknown index {other:?}")),
        }
    }

    fn refs(&mut self) -> Result<Vec<RefValue>, String> {
        let count = self.count("ref count")?;
        // A count is not trusted for an allocation: the fields must exist.
        let mut refs = Vec::new();
        for _ in 0..count {
            let name = RefName(self.text("ref name")?);
            let oid = match self.next("ref value")? {
                "-" => None,
                hex => Some(Oid::from_hex(hex).map_err(|e| format!("bad ref value: {e:?}"))?),
            };
            refs.push(RefValue { name, oid });
        }
        Ok(refs)
    }

    fn stash(&mut self) -> Result<StashEntry, String> {
        Ok(StashEntry {
            id: self.oid("stash id")?,
            message: self.text("stash message")?,
        })
    }
}

/// The entry a line stands for, or why the line is not one.
pub fn decode(line: &str) -> Result<UndoEntry, String> {
    let mut r = Reader(line.split('\t'));
    let version = r.next("version")?;
    if version != VERSION {
        return Err(format!("unknown format {version:?}"));
    }
    let field = r.next("op")?;
    let op: u64 = field.parse().map_err(|_| "bad op".to_owned())?;
    if op.to_string() != field {
        return Err("bad op".to_owned());
    }
    let op = OpId(op);
    let label = r.text("label")?;
    let head = r.head()?;
    let refs = r.refs()?;
    let index = r.index()?;
    let index_effect = match r.next("index effect")? {
        "rewrites" => IndexEffect::Rewrites,
        "untouched" => IndexEffect::Untouched,
        other => return Err(format!("unknown index effect {other:?}")),
    };
    let reverse = match r.next("reversal")? {
        "refs" => Reversal::Refs,
        "stash-store" => Reversal::StashStore(r.stash()?),
        "stash-kept" => Reversal::StashKept(r.stash()?),
        "never" => Reversal::Never(irreversible_of(r.next("reason")?)?),
        "discard" => Reversal::Discard {
            path: RepoPath(r.text("path")?),
        },
        other => return Err(format!("unknown reversal {other:?}")),
    };
    let tree = r.oid("tree")?;
    let after_refs = r.refs()?;
    let after_index = r.index()?;
    let after_head = r.head()?;
    let reversible = match r.next("reversible")? {
        "yes" => Reversible::Yes,
        "no" => Reversible::No(irreversible_of(r.next("reason")?)?),
        other => return Err(format!("unknown reversible {other:?}")),
    };
    if r.0.next().is_some() {
        return Err("extra fields at the end".to_owned());
    }
    Ok(UndoEntry {
        point: UndoPoint {
            op,
            label,
            head,
            refs,
            index,
            index_effect,
            reverse,
        },
        tree,
        after_refs,
        after_index,
        after_head,
        reversible,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn oid(c: char) -> Oid {
        Oid::from_hex(&c.to_string().repeat(40)).unwrap()
    }

    fn entry() -> UndoEntry {
        UndoEntry {
            point: UndoPoint {
                op: OpId(7),
                label: "Commit \"a\tb\"".into(),
                head: HeadTarget::Branch(BranchName("main".into())),
                refs: vec![
                    RefValue {
                        name: RefName("refs/heads/main".into()),
                        oid: Some(oid('a')),
                    },
                    RefValue {
                        name: RefName("refs/heads/new".into()),
                        oid: None,
                    },
                ],
                index: IndexId::Checksum(oid('b')),
                index_effect: IndexEffect::Rewrites,
                reverse: Reversal::Refs,
            },
            tree: oid('c'),
            after_refs: vec![RefValue {
                name: RefName("refs/heads/main".into()),
                oid: Some(oid('d')),
            }],
            after_index: IndexId::Absent,
            after_head: HeadTarget::Detached,
            reversible: Reversible::Yes,
        }
    }

    #[test]
    fn an_entry_goes_to_one_line_and_back() {
        let original = entry();
        let line = encode(&original);
        assert!(!line.contains('\n') && !line.contains('\r'));
        assert!(line.starts_with("u1\t7\t"));
        assert_eq!(decode(&line), Ok(original));
    }

    #[test]
    fn the_escapes_keep_separators_out_of_the_fields() {
        let mut e = entry();
        e.point.label = "a\\b\tc\nd\re\\t".into();
        e.point.reverse = Reversal::StashStore(StashEntry {
            id: oid('e'),
            message: "On main:\twip\r\n".into(),
        });
        let line = encode(&e);
        assert_eq!(line.matches('\t').count(), line.split('\t').count() - 1);
        assert_eq!(decode(&line), Ok(e));
    }

    #[test]
    fn a_line_that_is_not_exactly_an_entry_is_refused() {
        let good = encode(&entry());
        for bad in [
            String::new(),
            "u2\t1".to_owned(),
            good.replacen("u1", "u0", 1),
            format!("{good}\textra"),
            good.rsplit_once('\t').unwrap().0.to_owned(),
            good.replace("\tyes", "\tmaybe"),
            good.replace("\trewrites\t", "\tsometimes\t"),
            good.replace(&"a".repeat(40), "xyz"),
            "u1\t7\tlabel\tbranch\tmain\t99999\t".to_owned(),
        ] {
            assert!(decode(&bad).is_err(), "accepted {bad:?}");
        }
    }

    fn text() -> impl Strategy<Value = String> {
        "[a-z \\\\\t\n\r/\"]{0,12}"
    }

    fn oid_strategy() -> impl Strategy<Value = Oid> {
        "[0-9a-f]{40}".prop_map(|hex| Oid::from_hex(&hex).unwrap())
    }

    fn head() -> impl Strategy<Value = HeadTarget> {
        prop_oneof![
            text().prop_map(|name| HeadTarget::Branch(BranchName(name))),
            Just(HeadTarget::Detached),
        ]
    }

    fn index() -> impl Strategy<Value = IndexId> {
        prop_oneof![
            Just(IndexId::Absent),
            oid_strategy().prop_map(IndexId::Checksum)
        ]
    }

    fn refs() -> impl Strategy<Value = Vec<RefValue>> {
        proptest::collection::vec(
            (text(), proptest::option::of(oid_strategy())).prop_map(|(name, oid)| RefValue {
                name: RefName(name),
                oid,
            }),
            0..4,
        )
    }

    fn why() -> impl Strategy<Value = Irreversible> {
        prop_oneof![
            Just(Irreversible::Push),
            Just(Irreversible::StashRewroteFiles),
            Just(Irreversible::StashNotOnTop),
            Just(Irreversible::Pull),
            Just(Irreversible::MergeConcluded),
            Just(Irreversible::Continued),
        ]
    }

    fn stash() -> impl Strategy<Value = StashEntry> {
        (oid_strategy(), text()).prop_map(|(id, message)| StashEntry { id, message })
    }

    fn reversal() -> impl Strategy<Value = Reversal> {
        prop_oneof![
            Just(Reversal::Refs),
            stash().prop_map(Reversal::StashStore),
            stash().prop_map(Reversal::StashKept),
            why().prop_map(Reversal::Never),
            text().prop_map(|path| Reversal::Discard {
                path: RepoPath(path)
            }),
        ]
    }

    fn any_entry() -> impl Strategy<Value = UndoEntry> {
        let point = (
            any::<u64>(),
            text(),
            head(),
            refs(),
            index(),
            any::<bool>(),
            reversal(),
        )
            .prop_map(
                |(op, label, head, refs, index, rewrites, reverse)| UndoPoint {
                    op: OpId(op),
                    label,
                    head,
                    refs,
                    index,
                    index_effect: if rewrites {
                        IndexEffect::Rewrites
                    } else {
                        IndexEffect::Untouched
                    },
                    reverse,
                },
            );
        (
            point,
            oid_strategy(),
            refs(),
            index(),
            head(),
            proptest::option::of(why()),
        )
            .prop_map(
                |(point, tree, after_refs, after_index, after_head, no)| UndoEntry {
                    point,
                    tree,
                    after_refs,
                    after_index,
                    after_head,
                    reversible: no.map_or(Reversible::Yes, Reversible::No),
                },
            )
    }

    proptest! {
        /// Whatever an entry holds, it comes back equal from its one line.
        #[test]
        fn every_entry_round_trips(entry in any_entry()) {
            let line = encode(&entry);
            prop_assert!(!line.contains('\n') && !line.contains('\r'));
            prop_assert_eq!(decode(&line), Ok(entry));
        }

        /// No text makes the decoder panic, and a line it accepts encodes
        /// back to itself (so there is one spelling per entry).
        #[test]
        fn any_text_is_an_error_or_canonical(line in "[a-z0-9\\\\\t \\-]{0,200}") {
            if let Ok(entry) = decode(&line) {
                prop_assert_eq!(encode(&entry), line);
            }
        }

        /// Cutting a good line anywhere never gives a different entry.
        #[test]
        fn a_cut_line_is_refused(entry in any_entry(), cut in 0usize..400) {
            let line = encode(&entry);
            let cut = cut.min(line.len().saturating_sub(1));
            if line.is_char_boundary(cut) {
                prop_assert!(decode(&line[..cut]).is_err());
            }
        }
    }
}
