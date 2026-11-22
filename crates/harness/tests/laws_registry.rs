//! The law-ownership table (backlog task 24): every one of the 14 laws has
//! its owning test, and the test is still there.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use harness::laws::{LAWS, Law, Marker, Owner, TestRef, check_docs, check_laws};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

#[test]
fn every_law_has_its_owning_test() {
    let report = check_laws(&repo_root(), &LAWS);
    for note in &report.notes {
        eprintln!("pending: {note}");
    }
    assert!(report.errors.is_empty(), "{}", report.errors.join("\n"));
}

#[test]
fn the_table_matches_the_docs() {
    let errors = check_docs(&repo_root());
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
fn pending_laws_are_the_ones_whose_tasks_are_not_merged() {
    let pending: Vec<(u8, u8)> = LAWS
        .iter()
        .filter_map(|law| match law.owner {
            Owner::Pending { task, .. } => Some((law.number, task)),
            Owner::Test(_) => None,
        })
        .collect();
    // Law 7 waits for task 13 (discard).
    assert_eq!(pending, [(7, 13)]);
}

// The checks themselves, run on a small fake source tree, so the registry
// cannot rot into something that always passes.

fn fixture(files: &[(&str, &str)]) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "invoke-laws-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&dir);
    for (name, text) in files {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    dir
}

const fn one_law(owner: Owner, markers: &'static [Marker]) -> Law {
    Law {
        number: 1,
        summary: "fake",
        owner,
        also: &[],
        markers,
    }
}

fn run(root: &Path, law: Law) -> Vec<String> {
    // Only the table-shape error is ignored: the fake table has one law.
    check_laws(root, &[law])
        .errors
        .into_iter()
        .filter(|error| !error.starts_with("the table must list"))
        .collect()
}

const OWNER: TestRef = TestRef {
    file: "t.rs",
    name: "owner",
};

#[test]
fn a_renamed_owner_is_found_missing() {
    let ok = fixture(&[("t.rs", "#[test]\nfn owner() {}\n")]);
    assert!(run(&ok, one_law(Owner::Test(OWNER), &[])).is_empty());
    let renamed = fixture(&[("t.rs", "#[test]\nfn owner_renamed() {}\n")]);
    let errors = run(&renamed, one_law(Owner::Test(OWNER), &[]));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("no longer a running test"));
}

#[test]
fn an_ignored_or_deleted_owner_is_found() {
    let ignored = fixture(&[("t.rs", "#[test]\n#[ignore]\nfn owner() {}\n")]);
    assert_eq!(run(&ignored, one_law(Owner::Test(OWNER), &[])).len(), 1);
    let deleted = fixture(&[("other.rs", "")]);
    assert_eq!(run(&deleted, one_law(Owner::Test(OWNER), &[])).len(), 1);
}

#[test]
fn a_dropped_inline_check_is_found() {
    const MARKER: &[Marker] = &[Marker {
        file: "t.rs",
        text: "law 4:",
    }];
    let with = fixture(&[("t.rs", "#[test]\nfn owner() { panic!(\"law 4: x\") }\n")]);
    assert!(run(&with, one_law(Owner::Test(OWNER), MARKER)).is_empty());
    let without = fixture(&[("t.rs", "#[test]\nfn owner() {}\n")]);
    assert_eq!(run(&without, one_law(Owner::Test(OWNER), MARKER)).len(), 1);
}

#[test]
fn a_placeholder_must_be_promoted_when_its_test_lands() {
    let pending = Owner::Pending {
        task: 99,
        will_be: OWNER,
    };
    let absent = fixture(&[("t.rs", "")]);
    assert!(run(&absent, one_law(pending, &[])).is_empty());
    let landed = fixture(&[("t.rs", "#[test]\nfn owner() {}\n")]);
    let errors = run(&landed, one_law(pending, &[]));
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("make it the owner"));
}

#[test]
fn a_table_that_loses_a_law_is_found() {
    let report = check_laws(&repo_root(), &LAWS[..13]);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.starts_with("the table must list")),
        "{:?}",
        report.errors
    );
}
