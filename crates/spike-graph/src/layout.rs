//! Naive lane assignment from docs/spec/commit-graph.md ("Lane assignment"),
//! one commit at a time. Lanes keep their column for life (only the right end
//! is trimmed), so lanes that pass through a row are always straight.
//!
//! Only the edges that touch a commit's dot are stored. Lanes passing through
//! a row are rebuilt on demand: a snapshot of the open lanes every
//! [`CHECKPOINT`] rows, replayed forward with [`apply`]. On the kernel the
//! graph reaches ~700 open lanes, so storing pass-throughs per row would cost
//! hundreds of MB.

use std::ops::Range;

pub const COLORS: u8 = 8;
pub const CHECKPOINT: usize = 128;

/// Open lanes at a row boundary: index = column, value = lane colour.
pub type Lanes = Vec<Option<u8>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// From the top of the row at `from` into the commit's dot.
    In { from: u16, color: u8 },
    /// From the commit's dot to the bottom of the row at `to`.
    Out { to: u16, color: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub column: u16,
    pub color: u8,
    pub merge: bool,
    /// Range into the edge list (`In` edges first, then `Out`).
    pub edges: Range<u32>,
    /// Open lanes after this row, for the graph width and "+N" marker.
    pub width: u16,
}

#[derive(Clone, Copy)]
struct Lane<T> {
    waits_for: T,
    color: u8,
}

/// Layout state plus its output. `place` appends one row per commit; the
/// caller may drain `rows`, `edges` and `checkpoints` between pages and the
/// next page continues exactly where the last one stopped.
pub struct Layout<T> {
    lanes: Vec<Option<Lane<T>>>,
    next_color: u8,
    placed: usize,
    pub rows: Vec<Row>,
    pub edges: Vec<Edge>,
    /// Open lanes at the top of rows 0, CHECKPOINT, 2 × CHECKPOINT, …
    pub checkpoints: Vec<Lanes>,
}

impl<T: Copy + Eq> Default for Layout<T> {
    fn default() -> Self {
        Self {
            lanes: Vec::new(),
            next_color: 0,
            placed: 0,
            rows: Vec::new(),
            edges: Vec::new(),
            checkpoints: Vec::new(),
        }
    }
}

impl<T: Copy + Eq> Layout<T> {
    fn new_color(&mut self) -> u8 {
        let color = self.next_color;
        self.next_color = (self.next_color + 1) % COLORS;
        color
    }

    fn free_lane(&mut self) -> usize {
        match self.lanes.iter().position(Option::is_none) {
            Some(free) => free,
            None => {
                self.lanes.push(None);
                self.lanes.len() - 1
            }
        }
    }

    fn snapshot(&self) -> Lanes {
        self.lanes
            .iter()
            .map(|lane| lane.map(|l| l.color))
            .collect()
    }

    /// Places commit `id` with `parents` (first parent first) as the next row.
    pub fn place(&mut self, id: T, parents: &[T]) {
        if self.placed.is_multiple_of(CHECKPOINT) {
            self.checkpoints.push(self.snapshot());
        }
        self.placed += 1;
        let start = self.edges.len() as u32;
        let waiting: Vec<usize> = (0..self.lanes.len())
            .filter(|&i| self.lanes[i].is_some_and(|lane| lane.waits_for == id))
            .collect();

        // 1. The leftmost lane waiting for this commit is its column; a tip
        //    with no waiting lane takes the leftmost free lane.
        let (column, color) = match waiting.first() {
            Some(&first) => (first, self.lanes[first].map_or(0, |lane| lane.color)),
            None => (self.free_lane(), self.new_color()),
        };
        for &lane in &waiting {
            let color = self.lanes[lane].map_or(color, |lane| lane.color);
            self.edges.push(Edge::In {
                from: lane as u16,
                color,
            });
        }
        // 2. Every other lane that waited for this commit merges into it.
        for &lane in waiting.iter().skip(1) {
            self.lanes[lane] = None;
        }
        // 3. Continue down the first parent, or free the lane at a root.
        self.lanes[column] = parents.first().map(|&parent| {
            self.edges.push(Edge::Out {
                to: column as u16,
                color,
            });
            Lane {
                waits_for: parent,
                color,
            }
        });
        // 4. Further parents join a lane already waiting for them, or open one.
        for &parent in parents.iter().skip(1) {
            let existing = self
                .lanes
                .iter()
                .position(|lane| lane.is_some_and(|lane| lane.waits_for == parent));
            let (to, lane_color) = match existing {
                Some(lane) => (lane, self.lanes[lane].map_or(color, |lane| lane.color)),
                None => {
                    let lane = self.free_lane();
                    let lane_color = self.new_color();
                    self.lanes[lane] = Some(Lane {
                        waits_for: parent,
                        color: lane_color,
                    });
                    (lane, lane_color)
                }
            };
            self.edges.push(Edge::Out {
                to: to as u16,
                color: lane_color,
            });
        }
        // 5. Trim free lanes at the right end.
        while self.lanes.last().is_some_and(Option::is_none) {
            self.lanes.pop();
        }
        // 6. Emit the row.
        self.rows.push(Row {
            column: column as u16,
            color,
            merge: parents.len() > 1,
            edges: start..self.edges.len() as u32,
            width: self.lanes.len() as u16,
        });
    }
}

/// Advances `lanes` from the top of a row to its bottom, given that row's
/// edges: lanes entering the dot close, lanes leaving it open.
pub fn apply(lanes: &mut Lanes, edges: &[Edge]) {
    for edge in edges {
        if let Edge::In { from, .. } = *edge
            && let Some(lane) = lanes.get_mut(from as usize)
        {
            *lane = None;
        }
    }
    for edge in edges {
        if let Edge::Out { to, color } = *edge {
            let to = to as usize;
            if lanes.len() <= to {
                lanes.resize(to + 1, None);
            }
            lanes[to] = Some(color);
        }
    }
    while lanes.last().is_some_and(Option::is_none) {
        lanes.pop();
    }
}

/// Open lanes at the top of `row`, from the nearest checkpoint.
pub fn lanes_at(row: usize, rows: &[Row], edges: &[Edge], checkpoints: &[Lanes]) -> Lanes {
    let checkpoint = row / CHECKPOINT;
    let mut lanes = checkpoints[checkpoint].clone();
    for r in &rows[checkpoint * CHECKPOINT..row] {
        apply(
            &mut lanes,
            &edges[r.edges.start as usize..r.edges.end as usize],
        );
    }
    lanes
}

/// Lanes passing straight through a row: open at its top and not entering
/// its dot. Returns (column, colour).
pub fn passing(top: &Lanes, edges: &[Edge]) -> Vec<(u16, u8)> {
    top.iter()
        .enumerate()
        .filter_map(|(col, lane)| {
            let entering = edges
                .iter()
                .any(|e| matches!(*e, Edge::In { from, .. } if from as usize == col));
            match lane {
                Some(color) if !entering => Some((col as u16, *color)),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(commits: &[(u32, Vec<u32>)]) -> Layout<u32> {
        let mut layout = Layout::default();
        for (id, parents) in commits {
            layout.place(*id, parents);
        }
        layout
    }

    fn edges_of(l: &Layout<u32>, row: usize) -> &[Edge] {
        let r = &l.rows[row].edges;
        &l.edges[r.start as usize..r.end as usize]
    }

    /// Lanes must connect: every In edge has an open lane above it, every
    /// passing lane is still open below the row with the same colour, the
    /// checkpoint replay agrees with a straight replay, and all lanes close
    /// at the roots.
    fn assert_connected(l: &Layout<u32>) {
        let mut top = lanes_at(0, &l.rows, &l.edges, &l.checkpoints);
        for row in 0..l.rows.len() {
            assert_eq!(
                top,
                lanes_at(row, &l.rows, &l.edges, &l.checkpoints),
                "replay from checkpoint, row {row}"
            );
            let edges = edges_of(l, row);
            for e in edges {
                if let Edge::In { from, color } = *e {
                    assert_eq!(
                        top.get(from as usize),
                        Some(&Some(color)),
                        "row {row} In edge has a lane"
                    );
                }
            }
            let mut bottom = top.clone();
            apply(&mut bottom, edges);
            assert_eq!(bottom.len(), l.rows[row].width as usize, "row {row} width");
            for (col, color) in passing(&top, edges) {
                assert_eq!(
                    bottom.get(col as usize),
                    Some(&Some(color)),
                    "row {row} lane {col} passes through"
                );
            }
            top = bottom;
        }
        assert!(
            top.iter().all(Option::is_none),
            "all lanes close at the roots"
        );
    }

    #[test]
    fn linear_history_stays_in_one_column() {
        let l = layout(&[(3, vec![2]), (2, vec![1]), (1, vec![])]);
        assert!(l.rows.iter().all(|r| r.column == 0 && !r.merge));
        assert_eq!(l.rows[2].width, 0, "the root frees its lane");
        assert_connected(&l);
    }

    #[test]
    fn branch_and_merge_open_and_close_a_lane() {
        // 5 merges 4 (main) and 3 (feature); both come from 1.
        let l = layout(&[
            (5, vec![4, 3]),
            (4, vec![1]),
            (3, vec![2]),
            (2, vec![1]),
            (1, vec![]),
        ]);
        let columns: Vec<u16> = l.rows.iter().map(|r| r.column).collect();
        assert_eq!(columns, [0, 0, 1, 1, 0]);
        assert!(l.rows[0].merge);
        assert!(
            edges_of(&l, 4).contains(&Edge::In { from: 1, color: 1 }),
            "feature lane merges back"
        );
        let top = lanes_at(1, &l.rows, &l.edges, &l.checkpoints);
        assert_eq!(
            passing(&top, edges_of(&l, 1)),
            [(1, 1)],
            "feature lane passes main's commit"
        );
        assert_connected(&l);
    }

    #[test]
    fn octopus_fans_out_from_one_dot() {
        let l = layout(&[(9, vec![1, 2, 3]), (3, vec![]), (2, vec![]), (1, vec![])]);
        let outs = edges_of(&l, 0)
            .iter()
            .filter(|e| matches!(e, Edge::Out { .. }))
            .count();
        assert_eq!(outs, 3);
        assert_connected(&l);
    }

    #[test]
    fn two_tips_sharing_a_parent_merge_lanes() {
        let l = layout(&[(4, vec![2]), (3, vec![2]), (2, vec![1]), (1, vec![])]);
        assert_eq!(l.rows[1].column, 1, "second tip takes a free lane");
        assert_eq!(l.rows[2].column, 0);
        assert_eq!(
            edges_of(&l, 2)
                .iter()
                .filter(|e| matches!(e, Edge::In { .. }))
                .count(),
            2
        );
        assert_connected(&l);
    }

    #[test]
    fn colours_rotate_through_the_palette() {
        let tips: Vec<(u32, Vec<u32>)> = (0..10).map(|i| (100 + i, vec![])).collect();
        let colors: Vec<u8> = layout(&tips).rows.iter().map(|r| r.color).collect();
        assert_eq!(colors, [0, 1, 2, 3, 4, 5, 6, 7, 0, 1]);
    }

    /// Deterministic pseudo-random DAG: each commit has 1-3 parents among the
    /// next few older commits; long enough to cross many checkpoints.
    fn generated(n: u32) -> Vec<(u32, Vec<u32>)> {
        let mut seed = 0x9e37_79b9u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        (0..n)
            .map(|id| {
                let mut parents = match next() % 10 {
                    _ if id + 1 >= n => vec![],
                    0 => vec![id + 1, (id + 2 + next() % 20).min(n - 1)],
                    1 => vec![
                        id + 1,
                        (id + 2).min(n - 1),
                        (id + 3 + next() % 30).min(n - 1),
                    ],
                    _ => vec![id + 1],
                };
                parents.dedup();
                (id, parents)
            })
            .collect()
    }

    #[test]
    fn generated_histories_stay_connected() {
        let commits = generated(2_000);
        let l = layout(&commits);
        assert_eq!(l.rows.len(), commits.len(), "every commit placed once");
        assert!(l.rows.iter().any(|r| r.width > 3), "the history branches");
        assert_connected(&l);
    }

    #[test]
    fn paged_layout_equals_whole_layout() {
        let commits = generated(1_000);
        let whole = layout(&commits);
        // Lay out in pages of 300, draining output between pages as the walk
        // thread does, and rebase edge ranges onto the joined list.
        let mut paged: Layout<u32> = Layout::default();
        let (mut rows, mut edges, mut checkpoints) = (Vec::new(), Vec::new(), Vec::new());
        for page in commits.chunks(300) {
            for (id, parents) in page {
                paged.place(*id, parents);
            }
            let base = edges.len() as u32;
            rows.extend(std::mem::take(&mut paged.rows).into_iter().map(|mut r| {
                r.edges = r.edges.start + base..r.edges.end + base;
                r
            }));
            edges.extend(std::mem::take(&mut paged.edges));
            checkpoints.extend(std::mem::take(&mut paged.checkpoints));
        }
        assert_eq!(rows, whole.rows);
        assert_eq!(edges, whole.edges);
        assert_eq!(checkpoints, whole.checkpoints);
    }
}
