//! Criterion benchmarks for the commit-graph lane layout (backlog task 25).
//!
//! The histories are generated here from a fixed seed (nothing reads a real
//! repository), children first as the shell's topological walk would yield
//! them. Benchmark names are `graph/<name>` and must match a budget in
//! `docs/perf-budgets.toml`; `cargo xtask bench-check` runs this file and
//! fails past a budget.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use graph::{Commit, Layout, PAGE_SIZE};

/// splitmix64: tiny, deterministic, no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Shape of a generated history.
struct Shape {
    commits: usize,
    /// Parallel lines of development; each starts at its own root.
    branches: usize,
    /// Every this-many commits is a two-parent merge (0: never).
    merge_every: usize,
    /// Every this-many commits is an octopus merge (0: never).
    octopus_every: usize,
    /// Parents of an octopus merge.
    octopus_width: usize,
}

/// Builds the history oldest first, then reverses it, so every parent comes
/// after its children. Commit ids are their position in the returned list.
fn history(shape: &Shape) -> Vec<Commit<u32>> {
    let mut rng = Rng(0x0001_0bad_5eed);
    let n = shape.commits;
    let branches = shape.branches.min(n);
    // Parents by commit; the roots come first and have none.
    let mut parents: Vec<Vec<u32>> = vec![Vec::new(); branches];
    // The newest commit on each branch; the roots are the first commits.
    let mut heads: Vec<u32> = (0..branches as u32).collect();
    for k in branches..n {
        let b = rng.below(branches);
        let mut ps = vec![heads[b]];
        let extra = if shape.octopus_every != 0 && k % shape.octopus_every == 0 {
            shape.octopus_width.saturating_sub(1)
        } else if shape.merge_every != 0 && k % shape.merge_every == 0 {
            1
        } else {
            0
        };
        for _ in 0..extra {
            let other = heads[rng.below(branches)];
            if !ps.contains(&other) {
                ps.push(other);
            }
        }
        parents.push(ps);
        heads[b] = k as u32;
    }
    let flip = |id: u32| (n as u32 - 1) - id;
    (0..n)
        .rev()
        .map(|k| Commit {
            id: flip(k as u32),
            parents: parents[k].iter().map(|&p| flip(p)).collect(),
            truncated: false,
        })
        .collect()
}

fn lay_out(commits: &[Commit<u32>]) -> Layout<u32> {
    let mut layout = Layout::new();
    for page in commits.chunks(PAGE_SIZE) {
        layout.push_page(page);
    }
    layout
}

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph");

    let linear = history(&Shape {
        commits: 100_000,
        branches: 1,
        merge_every: 0,
        octopus_every: 0,
        octopus_width: 0,
    });
    // A busy project: a handful of branches that keep merging.
    let merging = history(&Shape {
        commits: 100_000,
        branches: 8,
        merge_every: 5,
        octopus_every: 0,
        octopus_width: 0,
    });
    // The spec's "wide" fixture: 200 branches with many merges, so a couple
    // of hundred lanes are open at once.
    let wide = history(&Shape {
        commits: 100_000,
        branches: 200,
        merge_every: 10,
        octopus_every: 0,
        octopus_width: 0,
    });
    // The same with an octopus merge of 12 parents every 50 commits.
    let octopus = history(&Shape {
        commits: 100_000,
        branches: 200,
        merge_every: 10,
        octopus_every: 50,
        octopus_width: 12,
    });

    // The generators must produce the shapes the names promise.
    let widest = |commits: &[Commit<u32>]| {
        let layout = lay_out(commits);
        let width = layout.rows().iter().map(|row| row.width).max().unwrap_or(0);
        let fan_out = layout
            .rows()
            .iter()
            .map(|row| layout.edges_out(row).len())
            .max()
            .unwrap_or(0);
        (width, fan_out)
    };
    assert_eq!(widest(&linear), (1, 1), "linear history is one lane");
    assert!(
        widest(&merging).0 >= 4,
        "merging history needs several lanes"
    );
    assert!(widest(&wide).0 >= 150, "wide history needs about 200 lanes");
    assert!(
        widest(&octopus).1 >= 12,
        "octopus history needs 12-parent merges"
    );

    for (name, commits) in [
        ("layout_100k_linear", &linear),
        ("layout_100k_merging", &merging),
        ("layout_100k_wide", &wide),
        ("layout_100k_octopus", &octopus),
    ] {
        group.bench_function(name, |b| b.iter(|| lay_out(black_box(commits))));
    }

    // One page: the first page of the wide history, lanes building up.
    group.bench_function("layout_page_5000_wide", |b| {
        b.iter(|| {
            let mut layout = Layout::new();
            layout.push_page(black_box(&wide[..PAGE_SIZE]));
            layout
        })
    });

    // What a renderer asks for on one frame: the pass-through lanes and edges
    // of 60 consecutive visible rows in the middle of a wide history.
    let layout = lay_out(&wide);
    group.bench_function("visible_window_60_wide", |b| {
        b.iter(|| {
            for index in 50_000..50_060 {
                let row = layout.row(black_box(index)).expect("row exists");
                black_box(layout.passing(index));
                black_box(layout.edges_in(row));
                black_box(layout.edges_out(row));
            }
        })
    });

    group.finish();
}

criterion_group! {
    name = layout_benches;
    // Short runs keep `cargo xtask bench-check` quick; the samples still give
    // a stable median for ceilings that have several times of headroom.
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(2));
    targets = benches
}
criterion_main!(layout_benches);
