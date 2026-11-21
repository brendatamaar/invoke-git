//! Myers O(ND) diff with the linear-space middle-snake refinement.
//!
//! Works on interned ids and writes its answer as "changed" flags, one per
//! element on each side. The lines left unflagged on both sides are equal and
//! pair up in order.

/// The work counter ran past its limit: the caller falls back to a plain
/// whole-file replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TooExpensive;

/// Reusable buffers and the work budget, shared by every call in one diff.
pub(crate) struct Ctx {
    vf: Vec<isize>,
    vb: Vec<isize>,
    used: u64,
    limit: u64,
}

impl Ctx {
    pub(crate) fn new(limit: u64) -> Self {
        Ctx {
            vf: Vec::new(),
            vb: Vec::new(),
            used: 0,
            limit,
        }
    }

    /// Charges `amount` units of work; errors once the limit is passed.
    pub(crate) fn spend(&mut self, amount: u64) -> Result<(), TooExpensive> {
        self.used = self.used.saturating_add(amount);
        if self.used > self.limit {
            Err(TooExpensive)
        } else {
            Ok(())
        }
    }
}

pub(crate) fn common_prefix(a: &[u32], b: &[u32]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

pub(crate) fn common_suffix(a: &[u32], b: &[u32]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
}

/// Flags the elements of `a` and `b` that are not part of a shortest common
/// subsequence. `ca` and `cb` must be the same lengths as `a` and `b`.
pub(crate) fn myers(
    a: &[u32],
    b: &[u32],
    ca: &mut [bool],
    cb: &mut [bool],
    cx: &mut Ctx,
) -> Result<(), TooExpensive> {
    debug_assert!(a.len() == ca.len() && b.len() == cb.len());
    let prefix = common_prefix(a, b);
    let suffix = common_suffix(&a[prefix..], &b[prefix..]);
    let (a, b) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let (ca, cb) = (
        &mut ca[prefix..prefix + a.len()],
        &mut cb[prefix..prefix + b.len()],
    );
    if a.is_empty() {
        cb.fill(true);
        return Ok(());
    }
    if b.is_empty() {
        ca.fill(true);
        return Ok(());
    }
    let (sx, sy, ex, ey) = middle_snake(a, b, cx)?;
    if (sx == a.len() && sy == b.len()) || (ex == 0 && ey == 0) {
        // No progress. Not expected, but a whole replace is always valid.
        ca.fill(true);
        cb.fill(true);
        return Ok(());
    }
    myers(&a[..sx], &b[..sy], &mut ca[..sx], &mut cb[..sy], cx)?;
    myers(&a[ex..], &b[ey..], &mut ca[ex..], &mut cb[ey..], cx)
}

/// Finds the middle snake of the edit graph: returns its start and end as
/// `(start_x, start_y, end_x, end_y)`. The path before the start and after the
/// end are solved separately.
fn middle_snake(
    a: &[u32],
    b: &[u32],
    cx: &mut Ctx,
) -> Result<(usize, usize, usize, usize), TooExpensive> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let delta = n - m;
    let odd = delta & 1 != 0;
    let max_d = (n + m + 1) / 2;
    let off = max_d + 1;
    let size = (2 * off + 1) as usize;
    if cx.vf.len() < size {
        cx.vf.resize(size, 0);
        cx.vb.resize(size, 0);
    }
    let at = |k: isize| (off + k) as usize;
    cx.vf[at(1)] = 0;
    cx.vb[at(1)] = 0;
    for d in 0..=max_d {
        for k in (-d..=d).step_by(2) {
            let mut x = if k == -d || (k != d && cx.vf[at(k - 1)] < cx.vf[at(k + 1)]) {
                cx.vf[at(k + 1)]
            } else {
                cx.vf[at(k - 1)] + 1
            };
            let mut y = x - k;
            let (x0, y0) = (x, y);
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            cx.used = cx.used.saturating_add(1 + (x - x0) as u64);
            cx.vf[at(k)] = x;
            if odd {
                let kb = delta - k;
                if kb > -d && kb < d && x + cx.vb[at(kb)] >= n {
                    return Ok((x0 as usize, y0 as usize, x as usize, y as usize));
                }
            }
        }
        for k in (-d..=d).step_by(2) {
            let mut x = if k == -d || (k != d && cx.vb[at(k - 1)] < cx.vb[at(k + 1)]) {
                cx.vb[at(k + 1)]
            } else {
                cx.vb[at(k - 1)] + 1
            };
            let mut y = x - k;
            let (x0, y0) = (x, y);
            while x < n && y < m && a[(n - 1 - x) as usize] == b[(m - 1 - y) as usize] {
                x += 1;
                y += 1;
            }
            cx.used = cx.used.saturating_add(1 + (x - x0) as u64);
            cx.vb[at(k)] = x;
            if !odd {
                let kf = delta - k;
                if kf >= -d && kf <= d && x + cx.vf[at(kf)] >= n {
                    return Ok((
                        (n - x) as usize,
                        (m - y) as usize,
                        (n - x0) as usize,
                        (m - y0) as usize,
                    ));
                }
            }
        }
        cx.spend(0)?;
    }
    Ok((a.len(), b.len(), a.len(), b.len()))
}
