//! Commit-graph lane layout (docs/spec/commit-graph.md, "Lane assignment").
//!
//! Pure: no IO, no clock, no randomness. The shell walks history in
//! topological order (children before parents) and hands commits to
//! [`Layout::push_page`] page by page. Each commit becomes one [`Row`]; row
//! `i` is the `i`-th commit pushed, so the caller keeps the id list.
//!
//! Input contract: commits arrive children first, each id at most once. A
//! parent that never arrives (outside the loaded pages, or beyond a shallow
//! boundary) simply keeps its lane open to the bottom.
//!
//! A lane keeps its column for life, so lanes that pass through a row are
//! always straight. Only the edges that touch a row's dot are stored; the
//! pass-through lanes are rebuilt on demand from a checkpoint (open lanes at
//! every [`CHECKPOINT`] rows) plus a short replay, so a renderer needs no
//! other history. See [`Layout::lanes_before`] and [`Layout::passing`].

mod layout;

pub use layout::{CHECKPOINT, COLORS, Commit, Edge, Lanes, Layout, PAGE_SIZE, Row};
