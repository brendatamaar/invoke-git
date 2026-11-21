//! Law 14 and the lane invariants, over random DAGs.
//!
//! The DAG generator numbers commits 0..n in topological order (children
//! first): parents of commit `i` have a larger number. Parent numbers
//! `>= n` are outside the loaded pages and never arrive. Every check reads
//! only the public output (rows, edges, checkpoints), the way a renderer
//! would.

use graph::{CHECKPOINT, COLORS, Commit, Layout};
use proptest::prelude::*;

fn dag() -> impl Strategy<Value = Vec<Commit<u32>>> {
    (1usize..300, 0u32..6).prop_flat_map(|(n, outside)| {
        let total = n as u32 + outside;
        let commits: Vec<_> = (0..n as u32)
            .map(|i| {
                // Mostly near parents (long-lived lanes), sometimes far ones.
                let near = (i + 1)..(i + 8);
                let any = (i + 1)..total.max(i + 2);
                let parent = prop_oneof![3 => near, 1 => any];
                (
                    proptest::collection::vec(parent, 0..5),
                    proptest::bool::weighted(0.05),
                )
                    .prop_map(move |(parents, truncated)| {
                        let mut seen = Vec::new();
                        for p in parents {
                            if p < total && !seen.contains(&p) {
                                seen.push(p);
                            }
                        }
                        Commit {
                            id: i,
                            parents: seen,
                            truncated,
                        }
                    })
            })
            .collect();
        commits
    })
}

fn layout_pages(commits: &[Commit<u32>], page: usize) -> Layout<u32> {
    let mut layout = Layout::new();
    for chunk in commits.chunks(page.max(1)) {
        layout.push_page(chunk);
    }
    layout
}

/// The parents a commit really has in the layout.
fn parents_of(commit: &Commit<u32>) -> &[u32] {
    if commit.truncated {
        &[]
    } else {
        &commit.parents
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Law 14: one row per commit, and every parent edge runs down to the
    /// row of that parent (or, for a parent that never arrives, to the
    /// bottom). Also: the lane never holds another commit on the way.
    #[test]
    fn edges_point_down_to_the_parent(commits in dag()) {
        let layout = layout_pages(&commits, 7);
        prop_assert_eq!(layout.len(), commits.len());
        let position = |id: u32| commits.iter().position(|c| c.id == id);
        for (r, commit) in commits.iter().enumerate() {
            let row = layout.row(r).unwrap();
            let outs = layout.edges_out(row);
            prop_assert_eq!(outs.len(), parents_of(commit).len());
            prop_assert_eq!(row.truncated, commit.truncated);
            prop_assert_eq!(row.merge, parents_of(commit).len() > 1);
            for (edge, &parent) in outs.iter().zip(parents_of(commit)) {
                // Walk down the lane until something arrives on it.
                let mut target = None;
                for s in r + 1..commits.len() {
                    let below = layout.row(s).unwrap();
                    let arrives = layout
                        .edges_in(below)
                        .iter()
                        .any(|e| e.column == edge.column);
                    if arrives {
                        target = Some(s);
                        break;
                    }
                    // A lane never holds two commits: no dot may sit on it
                    // while it waits.
                    prop_assert_ne!(below.column, edge.column);
                    let through = layout.passing(s).unwrap();
                    prop_assert!(through.contains(&(edge.column, edge.color)));
                }
                match (target, position(parent)) {
                    (Some(s), Some(p)) => {
                        prop_assert!(s > r, "edge must point down");
                        prop_assert_eq!(s, p, "edge ends at the parent's row");
                        let arriving = layout
                            .edges_in(layout.row(s).unwrap())
                            .iter()
                            .find(|e| e.column == edge.column)
                            .unwrap();
                        prop_assert_eq!(arriving.color, edge.color, "lane keeps its colour");
                    }
                    (None, None) => {} // parent outside the pages: lane stays open
                    (t, p) => prop_assert!(false, "edge ends at {:?}, parent at {:?}", t, p),
                }
            }
        }
    }

    /// Any page size gives the same rows, edges and lanes as one big page.
    #[test]
    fn paging_does_not_change_the_layout(commits in dag(), page in 1usize..200) {
        let whole = layout_pages(&commits, usize::MAX);
        let paged = layout_pages(&commits, page);
        prop_assert_eq!(whole.rows(), paged.rows());
        for row in whole.rows() {
            prop_assert_eq!(whole.edges_in(row), paged.edges_in(row));
            prop_assert_eq!(whole.edges_out(row), paged.edges_out(row));
        }
        prop_assert_eq!(whole.lane_targets(), paged.lane_targets());
        for i in (0..=commits.len()).step_by(CHECKPOINT / 2) {
            prop_assert_eq!(whole.lanes_before(i), paged.lanes_before(i));
        }
    }

    /// Replaying from checkpoints gives the lanes the layout really had.
    #[test]
    fn replayed_lanes_match_the_live_lanes(commits in dag()) {
        let mut layout = Layout::new();
        for (i, commit) in commits.iter().enumerate() {
            let live = layout.lane_targets();
            let colors = layout.lanes_before(i).unwrap();
            prop_assert_eq!(colors.len(), live.len());
            for (c, t) in colors.iter().zip(&live) {
                prop_assert_eq!(c.is_some(), t.is_some());
            }
            layout.place(commit);
        }
    }

    /// Lane placement step: every parent of a placed commit stays waited
    /// for by some lane until it is placed; a placed commit is waited for
    /// by none; the dot sits on a free lane or one waiting for it.
    #[test]
    fn waited_parents_stay_waited_for(commits in dag()) {
        let mut layout = Layout::new();
        let mut placed: Vec<u32> = Vec::new();
        let mut pending: Vec<u32> = Vec::new();
        for commit in &commits {
            let before = layout.lane_targets();
            layout.place(commit);
            // Step 4: a further parent that a lane already waits for is
            // joined there (the leftmost such lane), not given a new lane.
            let row = layout.row(placed.len()).unwrap();
            let outs = layout.edges_out(row);
            for (edge, p) in outs.iter().zip(parents_of(commit)).skip(1) {
                if let Some(column) = before.iter().position(|t| *t == Some(*p)) {
                    prop_assert_eq!(usize::from(edge.column), column);
                }
            }
            placed.push(commit.id);
            pending.retain(|&p| p != commit.id);
            for &p in parents_of(commit) {
                if !pending.contains(&p) {
                    pending.push(p);
                }
            }
            let targets = layout.lane_targets();
            for &p in &pending {
                prop_assert!(targets.contains(&Some(p)), "lane for {} lost", p);
            }
            for &id in &placed {
                prop_assert!(!targets.contains(&Some(id)), "lane still waits for placed {}", id);
            }
            // No stray lanes either.
            for t in targets.iter().flatten() {
                prop_assert!(pending.contains(t));
            }
            prop_assert!(targets.last().is_none_or(Option::is_some), "right end trimmed");
        }
    }

    /// Colours: in range, and a lane keeps one colour from its row to the
    /// row below that closes it (checked by `edges_point_down...`); here the
    /// dot takes the colour of the lane it arrives on.
    #[test]
    fn dot_colour_follows_its_lane(commits in dag()) {
        let layout = layout_pages(&commits, 64);
        for row in layout.rows() {
            prop_assert!(row.color < COLORS);
            let ins = layout.edges_in(row);
            if let Some(first) = ins.first() {
                prop_assert_eq!(first.color, row.color);
                prop_assert_eq!(first.column, ins.iter().map(|e| e.column).min().unwrap());
                prop_assert_eq!(row.column, first.column, "dot sits on the leftmost arriving lane");
            }
            let outs = layout.edges_out(row);
            if !row.truncated && let Some(first) = outs.first() {
                prop_assert_eq!(first.column, row.column);
                prop_assert_eq!(first.color, row.color);
            }
            let needed = layout
                .edges_in(row)
                .iter()
                .chain(outs)
                .map(|e| e.column + 1)
                .max()
                .unwrap_or(0)
                .max(row.column + 1);
            prop_assert!(row.width >= needed);
        }
    }
}

fn commit(id: u32, parents: &[u32]) -> Commit<u32> {
    Commit {
        id,
        parents: parents.to_vec(),
        truncated: false,
    }
}

#[test]
fn octopus_merge_fans_out_from_one_dot() {
    let mut layout = Layout::new();
    layout.push_page(&[
        commit(0, &[1, 2, 3, 4]),
        commit(1, &[]),
        commit(2, &[]),
        commit(3, &[]),
    ]);
    let top = layout.row(0).unwrap();
    let outs: Vec<u16> = layout.edges_out(top).iter().map(|e| e.column).collect();
    assert_eq!(outs, vec![0, 1, 2, 3]);
    assert!(top.merge);
    // Parent 4 never arrives: its lane is still open.
    assert_eq!(layout.lane_targets(), vec![None, None, None, Some(4)]);
}

#[test]
fn several_roots_sit_side_by_side() {
    let mut layout = Layout::new();
    layout.push_page(&[commit(0, &[]), commit(1, &[2]), commit(2, &[])]);
    assert_eq!(layout.row(0).unwrap().column, 0);
    assert_eq!(layout.row(1).unwrap().column, 0, "root freed its lane");
    assert!(layout.lane_targets().is_empty());
}

#[test]
fn shallow_tip_ends_its_lane_with_a_stub() {
    let mut layout = Layout::new();
    layout.push_page(&[Commit {
        id: 0,
        parents: vec![9],
        truncated: true,
    }]);
    let row = layout.row(0).unwrap();
    assert!(row.truncated);
    assert!(layout.edges_out(row).is_empty());
    assert!(layout.lane_targets().is_empty());
}

#[test]
fn parent_outside_the_pages_keeps_its_lane_across_a_page_edge() {
    let mut layout = Layout::new();
    layout.push_page(&[commit(0, &[5]), commit(1, &[5])]);
    assert_eq!(layout.lane_targets(), vec![Some(5), Some(5)]);
    layout.push_page(&[commit(5, &[])]);
    assert!(layout.lane_targets().is_empty());
    let row = layout.row(2).unwrap();
    assert_eq!(layout.edges_in(row).len(), 2);
}

/// Rough timing for 100k commits: `cargo test -p graph --release --
/// --ignored --nocapture`. Criterion benchmarks come with backlog task 25.
#[test]
#[ignore]
fn timing_100k_commits() {
    const N: u32 = 100_000;
    // A busy history: a feature branch forks every 10 commits and merges
    // back 7 commits later, so a few lanes are open at any time.
    let commits: Vec<Commit<u32>> = (0..N)
        .map(|i| {
            let mut parents = vec![i + 1];
            if i % 10 == 3 {
                parents.push(i + 7);
            }
            parents.retain(|&p| p < N);
            commit(i, &parents)
        })
        .collect();
    let start = std::time::Instant::now();
    let mut layout = Layout::new();
    for page in commits.chunks(graph::PAGE_SIZE) {
        layout.push_page(page);
    }
    let layout_time = start.elapsed();
    let start = std::time::Instant::now();
    for i in (0..commits.len()).step_by(997) {
        std::hint::black_box(layout.passing(i));
    }
    println!(
        "100k commits: layout {layout_time:?}; 100 passing() lookups {:?}",
        start.elapsed()
    );
}
