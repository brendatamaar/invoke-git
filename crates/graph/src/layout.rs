/// Lane colours rotate through this many palette entries.
pub const COLORS: u8 = 8;
/// A snapshot of the open lanes is kept every this many rows.
pub const CHECKPOINT: usize = 128;
/// Commits per page the shell's walk yields (spec: "Paging").
pub const PAGE_SIZE: usize = 5_000;

/// Open lanes at a row boundary: index = column, value = lane colour.
pub type Lanes = Vec<Option<u8>>;

/// One commit as the walk yields it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit<I> {
    pub id: I,
    /// First parent first.
    pub parents: Vec<I>,
    /// The commit sits on a shallow boundary: its real parents are missing.
    /// `parents` is ignored, the lane ends and the row is flagged so the
    /// renderer draws the faded "history truncated" stub.
    pub truncated: bool,
}

/// An edge touching a row's dot, stored as the column at the other end.
///
/// Arriving edges run from the top of the row at `column` into the dot (the
/// commit's own lane included, as a straight edge). Leaving edges run from
/// the dot to the bottom of the row at `column`; they appear in parent
/// order, so the first is the first parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub column: u16,
    pub color: u8,
}

/// One drawn row, 16 bytes. Edges live in a shared arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub column: u16,
    pub color: u8,
    /// More than one parent: drawn hollow.
    pub merge: bool,
    /// History ends here because of a shallow boundary: draw a stub.
    pub truncated: bool,
    /// Start of this row's edges in the arena: `in_count` arriving edges,
    /// then `out_count` leaving edges.
    edges_start: u32,
    in_count: u16,
    out_count: u16,
    /// Columns needed to draw this row: the widest of the dot, its edges and
    /// the lanes passing through.
    pub width: u16,
}

#[derive(Clone, Copy)]
struct Lane<I> {
    waits_for: I,
    color: u8,
}

/// Incremental layout. `push_page` appends rows and keeps the open-lane
/// state, so the next page continues exactly where the last one stopped.
pub struct Layout<I> {
    lanes: Vec<Option<Lane<I>>>,
    next_color: u8,
    rows: Vec<Row>,
    edges: Vec<Edge>,
    /// Open lanes at the top of rows 0, CHECKPOINT, 2 x CHECKPOINT, ...
    checkpoints: Vec<Box<[Option<u8>]>>,
}

impl<I: Copy + Eq> Default for Layout<I> {
    fn default() -> Self {
        Self::new()
    }
}

fn to_column(index: usize) -> u16 {
    // 65,535 simultaneous lanes is far beyond any real repository.
    u16::try_from(index).unwrap_or(u16::MAX)
}

fn to_u32(index: usize) -> u32 {
    u32::try_from(index).unwrap_or(u32::MAX)
}

impl<I: Copy + Eq> Layout<I> {
    pub fn new() -> Self {
        Self {
            lanes: Vec::new(),
            next_color: 0,
            rows: Vec::new(),
            edges: Vec::new(),
            checkpoints: Vec::new(),
        }
    }

    /// Lay out the next page of commits.
    pub fn push_page(&mut self, commits: &[Commit<I>]) {
        for commit in commits {
            self.place(commit);
        }
    }

    /// Lay out one commit (the spec's `place`).
    pub fn place(&mut self, commit: &Commit<I>) {
        if self.rows.len().is_multiple_of(CHECKPOINT) {
            self.checkpoints.push(self.open_colors().into_boxed_slice());
        }
        let edges_start = self.edges.len();

        // 1. Find the column: the leftmost lane waiting for this commit.
        let arriving: Vec<(usize, u8)> = self
            .lanes
            .iter()
            .enumerate()
            .filter_map(|(column, lane)| {
                lane.as_ref()
                    .filter(|lane| lane.waits_for == commit.id)
                    .map(|lane| (column, lane.color))
            })
            .collect();
        let (column, color) = match arriving.first() {
            Some(&found) => found,
            None => (self.free_column(), self.fresh_color()),
        };

        // 2. Close merging lanes: every arriving lane becomes an In edge.
        let mut width = column + 1;
        for &(from, lane_color) in &arriving {
            self.edges.push(Edge {
                column: to_column(from),
                color: lane_color,
            });
            self.lanes[from] = None;
            width = width.max(from + 1);
        }
        let in_count = arriving.len();

        // 3. Continue with the first parent; a root (or shallow tip) frees
        // the lane.
        let parents: &[I] = if commit.truncated {
            &[]
        } else {
            &commit.parents
        };
        if let Some(&first) = parents.first() {
            self.set_lane(column, first, color);
            self.edges.push(Edge {
                column: to_column(column),
                color,
            });
        }

        // 4. Other parents: join a lane already waiting, or open one.
        for &parent in parents.iter().skip(1) {
            let waiting = self.lanes.iter().enumerate().find_map(|(c, lane)| {
                lane.as_ref()
                    .filter(|lane| lane.waits_for == parent)
                    .map(|lane| (c, lane.color))
            });
            let (target, target_color) = match waiting {
                Some(found) => found,
                None => {
                    let target = self.free_column();
                    let target_color = self.fresh_color();
                    self.set_lane(target, parent, target_color);
                    (target, target_color)
                }
            };
            self.edges.push(Edge {
                column: to_column(target),
                color: target_color,
            });
        }

        // 5. Trim free lanes at the right end.
        while matches!(self.lanes.last(), Some(None)) {
            self.lanes.pop();
        }

        // 6. Emit the row. Every open lane (passing through, or the target
        // of a leaving edge) is inside the trimmed lane list.
        width = width.max(self.lanes.len());
        let out_count = self.edges.len() - edges_start - in_count;
        self.rows.push(Row {
            column: to_column(column),
            color,
            merge: parents.len() > 1,
            truncated: commit.truncated,
            edges_start: to_u32(edges_start),
            in_count: to_column(in_count),
            out_count: to_column(out_count),
            width: to_column(width),
        });
    }

    fn set_lane(&mut self, column: usize, waits_for: I, color: u8) {
        if column >= self.lanes.len() {
            self.lanes.resize_with(column + 1, || None);
        }
        self.lanes[column] = Some(Lane { waits_for, color });
    }

    /// Leftmost free lane, or one past the end.
    fn free_column(&self) -> usize {
        self.lanes
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.lanes.len())
    }

    fn fresh_color(&mut self) -> u8 {
        let color = self.next_color;
        self.next_color = (self.next_color + 1) % COLORS;
        color
    }

    fn open_colors(&self) -> Lanes {
        self.lanes
            .iter()
            .map(|lane| lane.as_ref().map(|lane| lane.color))
            .collect()
    }

    /// Number of rows laid out so far.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row(&self, index: usize) -> Option<&Row> {
        self.rows.get(index)
    }

    /// Edges arriving at the row's dot from above.
    pub fn edges_in(&self, row: &Row) -> &[Edge] {
        let start = row.edges_start as usize;
        &self.edges[start..start + usize::from(row.in_count)]
    }

    /// Edges leaving the row's dot downwards, in parent order.
    pub fn edges_out(&self, row: &Row) -> &[Edge] {
        let start = row.edges_start as usize + usize::from(row.in_count);
        &self.edges[start..start + usize::from(row.out_count)]
    }

    /// Ids the open lanes wait for, by column. For tests and for checking
    /// the lane invariant.
    pub fn lane_targets(&self) -> Vec<Option<I>> {
        self.lanes
            .iter()
            .map(|lane| lane.as_ref().map(|lane| lane.waits_for))
            .collect()
    }

    /// Open lanes (colours) at the top of row `index`, rebuilt from the
    /// nearest checkpoint. `index == len()` gives the lanes after the last
    /// row.
    pub fn lanes_before(&self, index: usize) -> Option<Lanes> {
        if index > self.rows.len() {
            return None;
        }
        let checkpoint = index / CHECKPOINT;
        let mut lanes: Lanes = match self.checkpoints.get(checkpoint) {
            Some(saved) => saved.to_vec(),
            // `index == len()` on a boundary: that checkpoint is only taken
            // when the next row is placed, so use the live state.
            None => self.open_colors(),
        };
        for row in &self.rows[checkpoint * CHECKPOINT..index] {
            self.apply(&mut lanes, row);
        }
        Some(lanes)
    }

    /// Move `lanes` from the top of `row` to its bottom.
    fn apply(&self, lanes: &mut Lanes, row: &Row) {
        for edge in self.edges_in(row) {
            lanes[usize::from(edge.column)] = None;
        }
        for edge in self.edges_out(row) {
            let column = usize::from(edge.column);
            if column >= lanes.len() {
                lanes.resize(column + 1, None);
            }
            lanes[column] = Some(edge.color);
        }
        while matches!(lanes.last(), Some(None)) {
            lanes.pop();
        }
    }

    /// Lanes passing straight through row `index` (open above and below, not
    /// touching the dot): `(column, colour)`, left to right.
    pub fn passing(&self, index: usize) -> Option<Vec<(u16, u8)>> {
        let row = self.rows.get(index)?;
        let lanes = self.lanes_before(index)?;
        let arriving = self.edges_in(row);
        Some(
            lanes
                .iter()
                .enumerate()
                .filter_map(|(column, lane)| {
                    let color = (*lane)?;
                    let column = to_column(column);
                    let touches = arriving.iter().any(|edge| edge.column == column);
                    (!touches).then_some((column, color))
                })
                .collect(),
        )
    }
}
