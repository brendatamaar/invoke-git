# Spec — Commit graph

## Data source and paging

The graph needs only parents and order to lay out, so it reads the commit-graph file first and loads messages and authors lazily for visible rows.

- **Starting points:** HEAD, local branches, remote branches and tags (stash hidden by default).
- **Walk:** gix revision walk in topological order (children before parents, like `git log --topo-order`), using generation numbers from the commit-graph file so the walk can stream without reading the whole history first.
- **Missing commit-graph:** fall back to parsing commit objects (slower) and show a one-time banner offering to run `git commit-graph write --reachable`.
- **Paging:** the walk yields pages of 5,000 commits; layout runs on each page as it arrives and saves its lane state at the page edge, so the next page continues without recomputing.
- **Lazy metadata:** subject, author, date and ref labels are read only for rows on screen plus a buffer, cached by commit id.
- **Row record:** commit index (`u32`), column (`u16`), and an offset into a shared edge arena; roughly 16 bytes a row plus edges, so 1M commits stay well under 100 MB.
- **Refresh:** when refs change, walk and layout restart from the top in the background; the old graph stays on screen until the first new page is ready, keeping the scroll position anchored to the same commit.

## Lane assignment

Layout keeps one array of open lanes, each holding the commit id it is waiting for, and processes commits top to bottom in a single pass.

```rust
struct LaneState { lanes: Vec<Option<ObjectId>> }  // index = column

fn place(state: &mut LaneState, c: &Commit) -> Row
```

For each commit `c`:

1. **Find its column.** Every lane waiting for `c` is a child edge arriving. The leftmost such lane becomes `c`'s column. If no lane waits for `c` (a branch tip), take the leftmost free lane, or append one.
2. **Close merging lanes.** The other lanes that were waiting for `c` draw an edge into `c`'s column and become free.
3. **Continue with the first parent.** `c`'s lane now waits for its first parent; if `c` has no parents (a root), the lane becomes free.
4. **Open lanes for other parents.** For each further parent `p`: if some lane already waits for `p`, draw an edge to it; otherwise put `p` in the leftmost free lane (or append) and draw an edge there.
5. **Trim** free lanes at the right end.
6. **Emit the row:** `c`'s column, the edges entering and leaving this row, and the lanes passing straight through.

**Properties the tests check** (law 14): every commit is placed exactly once; every edge runs from a child row to a row below it; a lane never holds two commits; layout of a page joined with the next equals layout of both at once.

**Colour:** a lane gets a colour when it opens and keeps it until it closes, so one branch line keeps one colour as it moves across columns.

## Rendering

The graph is a fixed-height virtualized list; each visible row draws its own slice of lanes with a custom GPUI element, so cost depends only on rows on screen.

- **Row:** 28 px tall; columns are graph, subject (with ref labels), author, date, short hash.
- **Lane width:** 14 px per column; the graph column grows to fit the widest visible row, capped at 16 lanes, with a "+N" marker when a row has more.
- **Per row, draw:** straight segments for passing lanes; the incoming half (top edge to dot) and outgoing half (dot to bottom edge) for the commit's own lane; quarter curves for edges that change column; the commit dot on top.
- **Dots:** filled circle for normal commits, hollow for merges, ring for HEAD; the selected row's dot gets the accent colour.
- **Ref labels:** pills before the subject: local branch, remote branch and tag styles from the tokens; the current branch is bold.
- **Colours:** a palette of 8 lane colours from the design tokens, picked in rotation as lanes open.
- **Hover:** a lane highlights end to end when hovered, so a long branch can be followed by eye.

## Edge cases

| Case | Handling |
| --- | --- |
| Octopus merge (3+ parents) | Step 4 runs once per extra parent; edges fan out from one dot |
| Several roots / orphan branches | A root frees its lane (step 3); separate histories sit side by side |
| Shallow clone | Parents beyond the shallow boundary are missing: the lane ends in a short faded stub with a "history truncated" tooltip |
| Hundreds of branches | Lanes beyond the cap are collapsed into the "+N" marker; a branch filter in the sidebar narrows the starting points |
| Commit dates out of order | Topological order is used, never date order, so clock skew cannot break layout |
| Replace refs and grafts | Ignored in v0.1 (shown as stored); revisit if users report it |
| Branch deleted while viewing | Ref refresh restarts layout; the selected commit stays selected if still reachable |
