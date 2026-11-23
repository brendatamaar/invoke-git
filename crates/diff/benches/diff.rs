//! Criterion benchmarks for the diff engine (backlog task 25).
//!
//! Every input is generated here from a fixed seed, so each machine times the
//! same bytes and nothing touches a real repository. Benchmark names are
//! `diff/<name>` and must match a budget in `docs/perf-budgets.toml`;
//! `cargo xtask bench-check` runs this file and fails past a budget.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use diff::{Algorithm, DiffOptions, diff_text, word_diff};

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

/// Lines of a Rust-like file: a third are common ones (braces, blanks), the
/// rest are mostly unique statements.
fn code_line(i: usize) -> String {
    if i.is_multiple_of(3) {
        ["}", "", "    return x;", "    }"][(i / 3) % 4].to_string()
    } else {
        format!("    let value_{} = compute({i}, {});", i % 977, i % 13)
    }
}

fn join(lines: &[String]) -> Arc<str> {
    let mut text = String::with_capacity(lines.iter().map(|line| line.len() + 1).sum());
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    text.into()
}

struct Pair {
    old: Arc<str>,
    new: Arc<str>,
    /// The modified lines as (old, new) pairs, for the word diff.
    modified: Vec<(String, String)>,
}

/// `lines` lines with `clusters` change clusters spread evenly. Each cluster
/// modifies 5 consecutive lines, deletes 2 and inserts 3, so 200 clusters in
/// 50k lines give about 2k changed lines.
fn typical(lines: usize, clusters: usize) -> Pair {
    let old: Vec<String> = (0..lines).map(code_line).collect();
    let stride = lines / clusters;
    let mut new = Vec::with_capacity(lines + clusters * 2);
    let mut modified = Vec::new();
    for (i, line) in old.iter().enumerate() {
        let at = i % stride;
        match at {
            // Five consecutive modified lines: one token changes.
            10..=14 => {
                let edited = format!("{} // edited {i}", line.replace("compute", "calculate"));
                modified.push((line.clone(), edited.clone()));
                new.push(edited);
            }
            // Two deleted lines.
            20 | 21 => {}
            // Three inserted lines after this one.
            30 => {
                new.push(line.clone());
                for k in 0..3 {
                    new.push(format!("    inserted_{i}_{k}();"));
                }
            }
            _ => new.push(line.clone()),
        }
    }
    Pair {
        old: join(&old),
        new: join(&new),
        modified,
    }
}

/// No line of the old file survives, and all lines are different from each
/// other: the cheap end of "everything changed".
fn all_changed(lines: usize) -> Pair {
    let old: Vec<String> = (0..lines).map(|i| format!("old line number {i}")).collect();
    let new: Vec<String> = (0..lines).map(|i| format!("new line number {i}")).collect();
    Pair {
        old: join(&old),
        new: join(&new),
        modified: Vec::new(),
    }
}

/// `lines` lines drawn from only `vocabulary` distinct lines, with about one
/// line in ten re-rolled in the new file. Every line occurs thousands of
/// times, which is the worst case for anchor search.
fn repeated(lines: usize, vocabulary: usize) -> Pair {
    let mut rng = Rng(0x0001_0bad_5eed);
    let word = |n: usize| format!("    repeated_line_{n}();");
    let old: Vec<String> = (0..lines).map(|_| word(rng.below(vocabulary))).collect();
    let new: Vec<String> = old
        .iter()
        .map(|line| {
            if rng.below(10) == 0 {
                word(rng.below(vocabulary))
            } else {
                line.clone()
            }
        })
        .collect();
    Pair {
        old: join(&old),
        new: join(&new),
        modified: Vec::new(),
    }
}

/// The new file is the old one reversed, over a tiny vocabulary: every line
/// changed position and almost everything can match something. This usually
/// runs out of work budget and falls back to a whole-file replace; the
/// benchmark times that path too.
fn reversed_repeated(lines: usize, vocabulary: usize) -> Pair {
    let mut rng = Rng(0x5eed_0001_0bad);
    let old: Vec<String> = (0..lines)
        .map(|_| format!("    repeated_line_{}();", rng.below(vocabulary)))
        .collect();
    let new: Vec<String> = old.iter().rev().cloned().collect();
    Pair {
        old: join(&old),
        new: join(&new),
        modified: Vec::new(),
    }
}

fn options(algorithm: Algorithm) -> DiffOptions {
    DiffOptions {
        algorithm,
        ..DiffOptions::default()
    }
}

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff");

    // The headline case: 50k lines, about 2k of them changed.
    let big = typical(50_000, 200);
    for (name, algorithm) in [
        ("histogram_50k_2k", Algorithm::Histogram),
        ("myers_50k_2k", Algorithm::Myers),
    ] {
        let opts = options(algorithm);
        group.bench_function(name, |b| {
            b.iter(|| diff_text(big.old.clone(), big.new.clone(), black_box(&opts)))
        });
    }

    // Hunk building alone: the diff is done once, the hunks are rebuilt with
    // more context, as the viewer's "expand" does.
    let done = diff_text(
        big.old.clone(),
        big.new.clone(),
        &options(Algorithm::Histogram),
    );
    group.bench_function("hunks_50k_2k", |b| {
        b.iter(|| black_box(&done).hunks_with_context(black_box(10)))
    });

    // PRD budget "stage a hunk to UI updated < 16 ms": the pure part of it,
    // building the patch for one hunk (and for a few lines of it) of the big
    // diff. The rest of the budget is `git apply` and the repaint.
    let middle = done.hunks.len() / 2;
    let spec = diff::PatchSpec {
        path: "src/lib.rs",
        direction: diff::Direction::Stage,
        old_exists: true,
        new_exists: true,
        mode: diff::Mode::Regular,
    };
    let changed: Vec<usize> = done.hunks[middle]
        .lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.kind != diff::LineKind::Context)
        .map(|(index, _)| index)
        .take(3)
        .collect();
    group.bench_function("stage_hunk_50k_2k", |b| {
        b.iter(|| diff::build_patch(black_box(&done), &spec, &diff::Pick::Hunk(middle)))
    });
    group.bench_function("stage_lines_50k_2k", |b| {
        b.iter(|| {
            diff::build_patch(
                black_box(&done),
                &spec,
                &diff::Pick::Lines {
                    hunk: middle,
                    lines: &changed,
                },
            )
        })
    });

    // Word diff of every modified pair, directly and through the hunks.
    group.bench_function("word_pairs_1k", |b| {
        b.iter(|| {
            for (old, new) in &big.modified {
                black_box(word_diff(old, new));
            }
        })
    });
    group.bench_function("fill_inline_all_hunks", |b| {
        b.iter_batched_ref(
            || done.clone(),
            |diff| {
                for hunk in 0..diff.hunks.len() {
                    diff.fill_inline(hunk);
                }
            },
            BatchSize::LargeInput,
        )
    });

    // PRD budget "open a 10k-line file diff": the diff plus the word diff of
    // the first hunk, which is what the viewer needs for its first lines.
    let small = typical(10_000, 40);
    let opts = options(Algorithm::Histogram);
    group.bench_function("open_10k_lines", |b| {
        b.iter(|| {
            let mut diff = diff_text(small.old.clone(), small.new.clone(), black_box(&opts));
            diff.fill_inline(0);
            diff
        })
    });

    // Worst cases.
    let worst = [
        ("all_changed_50k", all_changed(50_000)),
        ("repeated_10k", repeated(10_000, 24)),
        ("repeated_50k", repeated(50_000, 24)),
        ("reversed_repeated_20k", reversed_repeated(20_000, 8)),
    ];
    for (name, pair) in &worst {
        group.bench_function(*name, |b| {
            b.iter(|| diff_text(pair.old.clone(), pair.new.clone(), black_box(&opts)))
        });
    }
    // The same inputs through Myers, which the viewer offers as an option.
    let myers = options(Algorithm::Myers);
    let (name, pair) = &worst[2];
    group.bench_function(format!("myers_{name}"), |b| {
        b.iter(|| diff_text(pair.old.clone(), pair.new.clone(), black_box(&myers)))
    });

    group.finish();
}

criterion_group! {
    name = diff_benches;
    // Short runs keep `cargo xtask bench-check` quick; the samples still give
    // a stable median for ceilings that have several times of headroom.
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(2));
    targets = benches
}
criterion_main!(diff_benches);
