//! Turning changed flags into changes, and changes into hunks with context.

use crate::lines::{full_line, line_count, split_ending};
use crate::model::{Change, DiffLine, Eol, Hunk, LineKind, Stats, TextRange};

/// Walks both flag vectors in step and collects the changed regions.
pub(crate) fn changes_from_flags(ca: &[bool], cb: &[bool]) -> Vec<Change> {
    let (n, m) = (ca.len(), cb.len());
    let mut changes = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && !ca[i] && !cb[j] {
            i += 1;
            j += 1;
            continue;
        }
        let (a0, b0) = (i, j);
        while i < n && ca[i] {
            i += 1;
        }
        while j < m && cb[j] {
            j += 1;
        }
        if i == a0 && j == b0 {
            // Unequal counts of unchanged lines: cannot happen for a valid
            // flag pair. Treat the rest as one change rather than loop.
            i = n;
            j = m;
        }
        changes.push(Change {
            a0,
            a1: i,
            b0,
            b1: j,
        });
    }
    changes
}

pub(crate) struct Buffers<'a> {
    pub old: &'a str,
    pub old_starts: &'a [u32],
    pub new: &'a str,
    pub new_starts: &'a [u32],
}

impl Buffers<'_> {
    fn is_blank_change(&self, change: &Change) -> bool {
        (change.a0..change.a1).all(|i| full_line(self.old, self.old_starts, i).trim().is_empty())
            && (change.b0..change.b1)
                .all(|j| full_line(self.new, self.new_starts, j).trim().is_empty())
    }

    fn line(&self, kind: LineKind, old_idx: Option<usize>, new_idx: Option<usize>) -> DiffLine {
        let (text, starts, idx) = match kind {
            LineKind::Removed => (self.old, self.old_starts, old_idx),
            _ => (self.new, self.new_starts, new_idx),
        };
        let (start, end, eol): (u32, u32, Eol) = split_ending(text, starts, idx.unwrap_or(0));
        DiffLine {
            kind,
            old_no: old_idx.map(|i| i as u32 + 1),
            new_no: new_idx.map(|j| j as u32 + 1),
            text: TextRange { start, end },
            eol,
            inline: None,
        }
    }
}

/// Groups changes whose context would touch, and builds a hunk per group.
/// Returns the hunks and the changes that survived (`ignore_blank_lines` can
/// drop a group made only of blank-line changes).
pub(crate) fn build_hunks(
    changes: &[Change],
    buffers: &Buffers<'_>,
    context: usize,
    ignore_blank_lines: bool,
) -> (Vec<Hunk>, Stats) {
    let old_total = line_count(buffers.old_starts);
    let mut hunks = Vec::new();
    let mut stats = Stats::default();
    let mut index = 0;
    while index < changes.len() {
        let mut end = index + 1;
        while end < changes.len() && changes[end].a0 - changes[end - 1].a1 <= 2 * context {
            end += 1;
        }
        let group = &changes[index..end];
        index = end;
        if ignore_blank_lines && group.iter().all(|c| buffers.is_blank_change(c)) {
            continue;
        }
        let first = group[0];
        let last = group[group.len() - 1];
        let lead = context.min(first.a0);
        let trail = context.min(old_total - last.a1);
        let (mut i, mut j) = (first.a0 - lead, first.b0 - lead);
        let (old_idx0, new_idx0) = (i, j);
        let mut lines = Vec::new();
        let (mut old_len, mut new_len) = (0u32, 0u32);
        let context_run =
            |lines: &mut Vec<DiffLine>, count: usize, i: &mut usize, j: &mut usize| {
                for _ in 0..count {
                    lines.push(buffers.line(LineKind::Context, Some(*i), Some(*j)));
                    *i += 1;
                    *j += 1;
                }
            };
        context_run(&mut lines, lead, &mut i, &mut j);
        old_len += lead as u32;
        new_len += lead as u32;
        for change in group {
            let gap = change.a0 - i;
            context_run(&mut lines, gap, &mut i, &mut j);
            old_len += gap as u32;
            new_len += gap as u32;
            for k in change.a0..change.a1 {
                lines.push(buffers.line(LineKind::Removed, Some(k), None));
            }
            for k in change.b0..change.b1 {
                lines.push(buffers.line(LineKind::Added, None, Some(k)));
            }
            old_len += (change.a1 - change.a0) as u32;
            new_len += (change.b1 - change.b0) as u32;
            stats.removed += (change.a1 - change.a0) as u32;
            stats.added += (change.b1 - change.b0) as u32;
            i = change.a1;
            j = change.b1;
        }
        context_run(&mut lines, trail, &mut i, &mut j);
        old_len += trail as u32;
        new_len += trail as u32;
        hunks.push(Hunk {
            old_start: if old_len == 0 {
                old_idx0 as u32
            } else {
                old_idx0 as u32 + 1
            },
            old_len,
            new_start: if new_len == 0 {
                new_idx0 as u32
            } else {
                new_idx0 as u32 + 1
            },
            new_len,
            header: None,
            lines,
            inline_filled: false,
        });
    }
    (hunks, stats)
}
