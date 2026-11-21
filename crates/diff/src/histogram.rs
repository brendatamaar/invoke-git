//! Histogram diff, after git's `--histogram`.
//!
//! Finds the longest common run that contains the rarest line, splits the
//! region there and repeats on both sides. A region with no usable anchor
//! (every common line is too frequent, or none is common) goes to Myers.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::myers::{Ctx, TooExpensive, common_prefix, common_suffix, myers};

/// Lines that occur more often than this in the old region are not anchors.
const MAX_CHAIN: usize = 64;

/// Multiplicative hasher for dense `u32` ids; the default SipHash is wasted
/// on keys we generated ourselves.
#[derive(Default)]
pub(crate) struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
        }
    }
    fn write_u32(&mut self, value: u32) {
        self.0 = u64::from(value).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

pub(crate) type IdMap<V> = HashMap<u32, V, BuildHasherDefault<IdHasher>>;

struct Anchor {
    a_start: usize,
    a_end: usize,
    b_start: usize,
    b_end: usize,
}

pub(crate) fn histogram(
    a: &[u32],
    b: &[u32],
    ca: &mut [bool],
    cb: &mut [bool],
    cx: &mut Ctx,
) -> Result<(), TooExpensive> {
    // Regions as (a_lo, a_hi, b_lo, b_hi); a work list instead of recursion so
    // a long chain of anchors cannot overflow the stack.
    let mut work = vec![(0, a.len(), 0, b.len())];
    while let Some((mut a0, mut a1, mut b0, mut b1)) = work.pop() {
        let prefix = common_prefix(&a[a0..a1], &b[b0..b1]);
        a0 += prefix;
        b0 += prefix;
        let suffix = common_suffix(&a[a0..a1], &b[b0..b1]);
        a1 -= suffix;
        b1 -= suffix;
        if a0 == a1 {
            cb[b0..b1].fill(true);
            continue;
        }
        if b0 == b1 {
            ca[a0..a1].fill(true);
            continue;
        }
        cx.spend((a1 - a0 + b1 - b0) as u64)?;
        match find_anchor(&a[a0..a1], &b[b0..b1]) {
            Some(anchor) => {
                work.push((a0, a0 + anchor.a_start, b0, b0 + anchor.b_start));
                work.push((a0 + anchor.a_end, a1, b0 + anchor.b_end, b1));
            }
            None => myers(&a[a0..a1], &b[b0..b1], &mut ca[a0..a1], &mut cb[b0..b1], cx)?,
        }
    }
    Ok(())
}

/// The longest run of equal lines around the rarest common line, or `None`
/// when no line is rare enough to anchor on.
fn find_anchor(a: &[u32], b: &[u32]) -> Option<Anchor> {
    let mut occurrences: IdMap<Vec<u32>> = IdMap::default();
    for (index, id) in a.iter().enumerate() {
        occurrences.entry(*id).or_default().push(index as u32);
    }
    let count = |id: u32| occurrences.get(&id).map_or(0, Vec::len);

    let mut best: Option<Anchor> = None;
    let mut best_rarity = MAX_CHAIN;
    let mut bi = 0;
    while bi < b.len() {
        let Some(list) = occurrences.get(&b[bi]) else {
            bi += 1;
            continue;
        };
        if list.len() > best_rarity {
            bi += 1;
            continue;
        }
        let mut next_bi = bi + 1;
        for &start in list {
            let (mut sa, mut sb) = (start as usize, bi);
            let mut rarity = list.len();
            while sa > 0 && sb > 0 && a[sa - 1] == b[sb - 1] {
                sa -= 1;
                sb -= 1;
                rarity = rarity.min(count(a[sa]));
            }
            let (mut ea, mut eb) = (start as usize + 1, bi + 1);
            while ea < a.len() && eb < b.len() && a[ea] == b[eb] {
                rarity = rarity.min(count(a[ea]));
                ea += 1;
                eb += 1;
            }
            next_bi = next_bi.max(eb);
            // Longer wins; on a tie the match nearer the middle of the region
            // wins, so equal-sized runs split the work in half instead of
            // peeling one run at a time.
            let middle = |start: usize, end: usize| (start + end).abs_diff(b.len());
            let longer = best.as_ref().is_none_or(|m| {
                let (len, best_len) = (ea - sa, m.a_end - m.a_start);
                len > best_len
                    || (len == best_len
                        && rarity <= best_rarity
                        && middle(sb, eb) < middle(m.b_start, m.b_end))
            });
            if longer || rarity < best_rarity {
                best_rarity = best_rarity.min(rarity);
                best = Some(Anchor {
                    a_start: sa,
                    a_end: ea,
                    b_start: sb,
                    b_end: eb,
                });
            }
        }
        bi = next_bi;
    }
    best
}
