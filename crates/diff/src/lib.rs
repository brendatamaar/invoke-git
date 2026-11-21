//! Pure diff engine: no IO, clock, threads or randomness.
//!
//! `diff_text(old, new, &options)` interns lines to ids, runs histogram (or
//! Myers), slides changes to block edges, and groups them into hunks with
//! context. See `docs/spec/diff-engine.md`.

mod apply;
mod histogram;
mod hunks;
mod lines;
mod model;
mod myers;
mod patch;
mod slide;
mod word;

use std::sync::Arc;

pub use apply::ApplyError;
pub use model::{
    Algorithm, Change, DiffLine, DiffNote, DiffOptions, Eol, Hunk, InlineSpans, LineKind, Stats,
    TextDiff, TextRange, Whitespace,
};
pub use patch::{Direction, Mode, PatchError, PatchSpec, Pick, build_patch};
pub use word::word_diff;

use hunks::{Buffers, build_hunks, changes_from_flags};
use lines::{intern, line_count, line_starts};
use myers::{Ctx, TooExpensive};

/// Diffs two texts. Accepts `&str`, `String` or `Arc<str>`; an `Arc` is
/// shared, not copied.
pub fn diff_text(
    old: impl Into<Arc<str>>,
    new: impl Into<Arc<str>>,
    options: &DiffOptions,
) -> TextDiff {
    let old: Arc<str> = old.into();
    let new: Arc<str> = new.into();
    if u32::try_from(old.len()).is_err() || u32::try_from(new.len()).is_err() {
        return finish(
            old,
            new,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            options,
            Some(DiffNote::InputTooLarge),
        );
    }
    let old_starts = line_starts(&old);
    let new_starts = line_starts(&new);
    let (n, m) = (line_count(&old_starts), line_count(&new_starts));

    let (a, b) = intern(&old, &old_starts, &new, &new_starts, options.whitespace);
    let mut ca = vec![false; n];
    let mut cb = vec![false; m];
    let mut cx = Ctx::new(options.max_work);
    let result = run(options.algorithm, &a, &b, &mut ca, &mut cb, &mut cx);
    let (changes, note) = match result {
        Ok(()) => {
            if options.indent_heuristic {
                slide::slide(&a, &slide::indents(&old, &old_starts), &mut ca);
                slide::slide(&b, &slide::indents(&new, &new_starts), &mut cb);
            }
            (changes_from_flags(&ca, &cb), None)
        }
        Err(TooExpensive) => {
            let whole = Change {
                a0: 0,
                a1: n,
                b0: 0,
                b1: m,
            };
            let changes = if n + m == 0 { Vec::new() } else { vec![whole] };
            (changes, Some(DiffNote::TooExpensive))
        }
    };
    finish(old, new, old_starts, new_starts, changes, options, note)
}

/// Flags changed lines. Lines whose id occurs only on one side can never be
/// matched, so they are flagged up front and left out of the real diff; this
/// keeps the anchor search from rescanning the whole file once per change.
fn run(
    algorithm: Algorithm,
    a: &[u32],
    b: &[u32],
    ca: &mut [bool],
    cb: &mut [bool],
    cx: &mut Ctx,
) -> Result<(), TooExpensive> {
    let ids = a.iter().chain(b).max().map_or(0, |max| *max as usize + 1);
    let mut in_a = vec![false; ids];
    let mut in_b = vec![false; ids];
    for &id in a {
        in_a[id as usize] = true;
    }
    for &id in b {
        in_b[id as usize] = true;
    }
    // Indexes of the lines that survive, and their ids.
    let keep_a: Vec<usize> = (0..a.len()).filter(|&i| in_b[a[i] as usize]).collect();
    let keep_b: Vec<usize> = (0..b.len()).filter(|&j| in_a[b[j] as usize]).collect();
    let small_a: Vec<u32> = keep_a.iter().map(|&i| a[i]).collect();
    let small_b: Vec<u32> = keep_b.iter().map(|&j| b[j]).collect();
    let mut small_ca = vec![false; small_a.len()];
    let mut small_cb = vec![false; small_b.len()];
    cx.spend((a.len() + b.len()) as u64)?;
    match algorithm {
        Algorithm::Histogram => {
            histogram::histogram(&small_a, &small_b, &mut small_ca, &mut small_cb, cx)?
        }
        Algorithm::Myers => myers::myers(&small_a, &small_b, &mut small_ca, &mut small_cb, cx)?,
    }
    ca.fill(true);
    cb.fill(true);
    for (&i, &changed) in keep_a.iter().zip(&small_ca) {
        ca[i] = changed;
    }
    for (&j, &changed) in keep_b.iter().zip(&small_cb) {
        cb[j] = changed;
    }
    Ok(())
}

fn finish(
    old: Arc<str>,
    new: Arc<str>,
    old_starts: Vec<u32>,
    new_starts: Vec<u32>,
    changes: Vec<Change>,
    options: &DiffOptions,
    note: Option<DiffNote>,
) -> TextDiff {
    let mut diff = TextDiff {
        old,
        new,
        hunks: Vec::new(),
        stats: Stats::default(),
        note,
        changes,
        old_starts,
        new_starts,
        ignore_blank_lines: options.ignore_blank_lines,
    };
    let (hunks, stats) = diff.hunks_for(options.context as usize);
    diff.hunks = hunks;
    diff.stats = stats;
    diff
}

impl TextDiff {
    /// The hunks with `context` lines around each change, without recomputing
    /// the diff (the viewer's "expand" asks for this).
    pub fn hunks_with_context(&self, context: u32) -> Vec<Hunk> {
        self.hunks_for(context as usize).0
    }

    fn hunks_for(&self, context: usize) -> (Vec<Hunk>, Stats) {
        let buffers = Buffers {
            old: &self.old,
            old_starts: &self.old_starts,
            new: &self.new,
            new_starts: &self.new_starts,
        };
        build_hunks(&self.changes, &buffers, context, self.ignore_blank_lines)
    }
}
