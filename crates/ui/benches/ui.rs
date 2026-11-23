//! Criterion benchmarks for the parts of the UI that are plain functions
//! (backlog task 25): the status tree's rows and the diff viewer's rows.
//! Everything is generated here from a formula, so each machine times the
//! same data. Benchmark names are `ui/<name>` and must match a budget in
//! `docs/perf-budgets.toml`; `cargo xtask bench-check --ui` runs this file.
//!
//! GPUI is linked but never started: nothing here opens a window.

use std::collections::HashSet;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use diff::{DiffOptions, diff_text};
use invoke_core::{FileChange, RepoPath, StatusEntry, StatusSnapshot};
use ui::diff_rows::{Layout, rows};
use ui::tree_rows;

/// `count` changed files in a tree of folders: `d{a}/s{b}/f{n}.rs`, 100 files
/// to a folder at the bottom, as in a large source tree. Every tenth file is
/// staged as well.
fn status(count: usize) -> StatusSnapshot {
    StatusSnapshot::from_entries((0..count).map(|n| StatusEntry {
        path: RepoPath(format!("d{}/s{}/f{n}.rs", n / 10_000, n / 100)),
        unstaged: Some(FileChange::Modified),
        staged: (n % 10 == 0).then_some(FileChange::Modified),
    }))
}

fn lines(count: usize, edit: impl Fn(usize) -> Option<String>) -> Arc<str> {
    let mut text = String::new();
    for i in 0..count {
        match edit(i) {
            Some(line) => text.push_str(&line),
            None => text.push_str(&format!("    let value_{} = compute({i});", i % 977)),
        }
        text.push('\n');
    }
    text.into()
}

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("ui");
    let none = HashSet::new();

    // PRD: "status refresh after a file save, 100k-file repo < 100 ms". This
    // is the rows of the tree for a status that big (the worst case: every
    // file changed), and for the 2k files a big change really touches.
    let huge = status(100_000);
    group.bench_function("tree_rows_100k", |b| {
        b.iter(|| tree_rows(black_box(&huge), &none))
    });
    let big = status(2_000);
    group.bench_function("tree_rows_2k", |b| {
        b.iter(|| tree_rows(black_box(&big), &none))
    });

    // PRD: "open a 10k-line file diff < 50 ms to first visible lines". The
    // diff itself is `diff/open_10k_lines`; these are the rows the viewer
    // lays out from it, split and unified, for 40 change clusters, and for a
    // file in which every line changed (the most pairing work).
    let old = lines(10_000, |_| None);
    let new = lines(10_000, |i| {
        (i % 250 < 5).then(|| format!("    let edited_{i} = calculate({i});"))
    });
    let clustered = diff_text(old.clone(), new, &DiffOptions::default());
    group.bench_function("rows_split_10k", |b| {
        b.iter(|| rows(black_box(&clustered), Layout::Split))
    });
    group.bench_function("rows_unified_10k", |b| {
        b.iter(|| rows(black_box(&clustered), Layout::Unified))
    });
    let rewritten = lines(10_000, |i| Some(format!("    other_{i}();")));
    let everything = diff_text(old, rewritten, &DiffOptions::default());
    group.bench_function("rows_split_all_changed_10k", |b| {
        b.iter(|| rows(black_box(&everything), Layout::Split))
    });

    group.finish();
}

criterion_group! {
    name = ui_benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(2));
    targets = benches
}
criterion_main!(ui_benches);
