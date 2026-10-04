//! The five filter types of filter method 0 (PNG §9) and the encoder's
//! choice among them.

/// A filter type (PNG §9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Filter {
    /// 0: the bytes as they are.
    None,
    /// 1: minus the byte one pixel to the left.
    Sub,
    /// 2: minus the byte above.
    Up,
    /// 3: minus the floor of the mean of left and above.
    Average,
    /// 4: minus the Paeth predictor of left, above and upper left.
    Paeth,
}

impl Filter {
    /// All five, in type order.
    pub const ALL: [Filter; 5] = [Filter::None, Filter::Sub, Filter::Up, Filter::Average, Filter::Paeth];

    /// The filter type byte.
    pub fn code(self) -> u8 {
        self as u8
    }

    /// The filter with type byte `code`.
    pub fn from_code(code: u8) -> Option<Filter> {
        Filter::ALL.get(code as usize).copied()
    }
}

/// How the encoder filters each row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FilterStrategy {
    /// The same filter on every row.
    Fixed(Filter),
    /// Per row, the filter whose output has the smallest sum of absolute
    /// values (bytes read as signed), the heuristic PNG §12.7 suggests; and
    /// None for indexed images and images below eight bits, as it also
    /// suggests.
    #[default]
    Adaptive,
    /// Per row, the filter whose output is cheapest by the same measure,
    /// including indexed and low-depth images.
    AdaptiveAlways,
}

/// The Paeth predictor (PNG §9.4).
#[inline]
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let pa = (p - a as i16).abs();
    let pb = (p - b as i16).abs();
    let pc = (p - c as i16).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Reverses `filter` on `row` in place, given the previous reconstructed row
/// (`prev`, all zeros for the first row of an image or pass) and the
/// distance in bytes to the corresponding byte of the previous pixel (`bpp`,
/// at least 1).
pub(crate) fn unfilter(filter: Filter, row: &mut [u8], prev: &[u8], bpp: usize) {
    match filter {
        Filter::None => {}
        Filter::Up => {
            for (x, &b) in row.iter_mut().zip(prev) {
                *x = x.wrapping_add(b);
            }
        }
        f => {
            if !crate::simd::unfilter(f.code(), row, prev, bpp) {
                unfilter_scalar_from(f.code(), row, prev, bpp, 0);
            }
        }
    }
}

/// The portable reverse filter (type `kind`) from byte `start` on; the
/// bytes before `start` are already reconstructed. The vector kernels
/// finish rows with this.
pub(crate) fn unfilter_scalar_from(kind: u8, row: &mut [u8], prev: &[u8], bpp: usize, start: usize) {
    let n = row.len();
    let head = bpp.min(n);
    match kind {
        1 => {
            for i in start.max(bpp)..n {
                row[i] = row[i].wrapping_add(row[i - bpp]);
            }
        }
        2 => {
            for i in start..n {
                row[i] = row[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in start..head {
                row[i] = row[i].wrapping_add(prev[i] / 2);
            }
            for i in start.max(head)..n {
                row[i] = row[i].wrapping_add(((row[i - bpp] as u16 + prev[i] as u16) / 2) as u8);
            }
        }
        4 => {
            // With no pixel to the left, a and c are 0 and Paeth picks b.
            for i in start..head {
                row[i] = row[i].wrapping_add(prev[i]);
            }
            for i in start.max(head)..n {
                row[i] = row[i].wrapping_add(paeth(row[i - bpp], prev[i], prev[i - bpp]));
            }
        }
        _ => {}
    }
}

/// Applies `filter` to `row` (previous row `prev`), writing to `out`.
/// Written as one loop per filter over whole slices, without branches in
/// the bodies, so that the compiler vectorises them.
#[inline(always)]
pub(crate) fn apply(filter: Filter, row: &[u8], prev: &[u8], bpp: usize, out: &mut [u8]) {
    let n = row.len();
    let k = bpp.min(n);
    let (prev, out) = (&prev[..n], &mut out[..n]);
    match filter {
        Filter::None => out.copy_from_slice(row),
        Filter::Sub => {
            out[..k].copy_from_slice(&row[..k]);
            for ((o, &x), &a) in out[k..].iter_mut().zip(&row[k..]).zip(&row[..n - k]) {
                *o = x.wrapping_sub(a);
            }
        }
        Filter::Up => {
            for ((o, &x), &b) in out.iter_mut().zip(row).zip(prev) {
                *o = x.wrapping_sub(b);
            }
        }
        Filter::Average => {
            for i in 0..k {
                out[i] = row[i].wrapping_sub(prev[i] / 2);
            }
            for (((o, &x), &a), &b) in out[k..].iter_mut().zip(&row[k..]).zip(&row[..n - k]).zip(&prev[k..]) {
                *o = x.wrapping_sub(((a as u16 + b as u16) >> 1) as u8);
            }
        }
        Filter::Paeth => {
            for i in 0..k {
                out[i] = row[i].wrapping_sub(prev[i]);
            }
            for ((((o, &x), &a), &b), &c) in
                out[k..].iter_mut().zip(&row[k..]).zip(&row[..n - k]).zip(&prev[k..]).zip(&prev[..n - k])
            {
                *o = x.wrapping_sub(paeth_select(a, b, c));
            }
        }
    }
}

/// The Paeth predictor without branches (the same choice as [`paeth`]).
#[inline(always)]
fn paeth_select(a: u8, b: u8, c: u8) -> u8 {
    let (a16, b16, c16) = (a as i16, b as i16, c as i16);
    let pa = (b16 - c16).abs();
    let pb = (a16 - c16).abs();
    let pc = (a16 + b16 - 2 * c16).abs();
    let bc = if pb <= pc { b } else { c };
    if pa <= pb && pa <= pc { a } else { bc }
}

#[inline(always)]
fn cost(bytes: &[u8]) -> u64 {
    // Summed in 32-bit lanes, a piece at a time, so that it vectorises.
    bytes.chunks(1 << 16).map(|p| p.iter().map(|&b| (b as i8).unsigned_abs() as u32).sum::<u32>() as u64).sum()
}

/// Filters one row into `best` (with `trial` as scratch) and returns the
/// filter chosen.
#[inline(always)]
fn filter_row(
    row: &[u8],
    prev: &[u8],
    bpp: usize,
    strategy: FilterStrategy,
    low_depth_or_indexed: bool,
    trial: &mut Vec<u8>,
    best: &mut Vec<u8>,
) -> Filter {
    match strategy {
        FilterStrategy::Fixed(f) => {
            apply(f, row, prev, bpp, best);
            f
        }
        FilterStrategy::Adaptive if low_depth_or_indexed => {
            best.copy_from_slice(row);
            Filter::None
        }
        FilterStrategy::Adaptive | FilterStrategy::AdaptiveAlways => {
            let mut chosen = Filter::None;
            let mut chosen_cost = u64::MAX;
            for f in Filter::ALL {
                apply(f, row, prev, bpp, trial);
                let c = cost(trial);
                if c < chosen_cost {
                    chosen_cost = c;
                    chosen = f;
                    std::mem::swap(trial, best);
                }
            }
            chosen
        }
    }
}

/// Filters rows `range` of `rows` (each `row_len` bytes), appending each
/// with its filter type byte to `out`.
fn filter_range(
    rows: &[u8],
    row_len: usize,
    bpp: usize,
    strategy: FilterStrategy,
    low_depth_or_indexed: bool,
    range: std::ops::Range<usize>,
    out: &mut Vec<u8>,
) {
    let body = |out: &mut Vec<u8>| {
        let zero = vec![0u8; row_len];
        let mut trial = vec![0u8; row_len];
        let mut best = vec![0u8; row_len];
        for y in range {
            let row = &rows[y * row_len..(y + 1) * row_len];
            let prev = if y == 0 { &zero[..] } else { &rows[(y - 1) * row_len..y * row_len] };
            let choice = filter_row(row, prev, bpp, strategy, low_depth_or_indexed, &mut trial, &mut best);
            out.push(choice.code());
            out.extend_from_slice(&best);
        }
    };
    crate::simd::with_wide_vectors(|| body(out))
}

/// Rows per task when filtering in parallel: enough to amortise a thread,
/// few enough to share out a picture of a few megapixels.
const PARALLEL_BYTES: usize = 1 << 18;

/// Filters `rows` (each `row_len` bytes) into `out`, with a filter type byte
/// before each row. Rows are filtered from the unfiltered rows above them,
/// so they are independent: large images are shared among `threads`
/// threads (0: as many as the machine has), with the same result.
pub(crate) fn filter_rows(
    rows: &[u8],
    row_len: usize,
    bpp: usize,
    strategy: FilterStrategy,
    low_depth_or_indexed: bool,
    threads: usize,
    out: &mut Vec<u8>,
) {
    if row_len == 0 {
        return;
    }
    let height = rows.len() / row_len;
    let per_task = (PARALLEL_BYTES / row_len).max(1);
    let tasks = height.div_ceil(per_task);
    if tasks <= 1 {
        filter_range(rows, row_len, bpp, strategy, low_depth_or_indexed, 0..height, out);
        return;
    }
    let parts = crate::par::map(tasks, threads, |t| {
        let range = t * per_task..((t + 1) * per_task).min(height);
        let mut part = Vec::with_capacity(range.len() * (row_len + 1));
        filter_range(rows, row_len, bpp, strategy, low_depth_or_indexed, range, &mut part);
        part
    });
    for p in parts {
        out.extend_from_slice(&p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_filter_inverts() {
        let bpp = 3;
        let prev: Vec<u8> = (0..30).map(|i| (i * 37 % 256) as u8).collect();
        let row: Vec<u8> = (0..30).map(|i| (i * 91 % 256 + 3) as u8).collect();
        for f in Filter::ALL {
            let mut out = vec![0; 30];
            apply(f, &row, &prev, bpp, &mut out);
            unfilter(f, &mut out, &prev, bpp);
            assert_eq!(out, row, "{f:?}");
        }
        assert_eq!(paeth(10, 20, 15), 15);
        assert_eq!(paeth(10, 20, 10), 20);
        assert_eq!(paeth(10, 20, 20), 10);
    }

    #[test]
    fn apply_matches_the_definition() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut byte = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        };
        for bpp in 1..=8 {
            for len in [1usize, 3, 7, 16, 33, 100, 257] {
                let row: Vec<u8> = (0..len).map(|_| byte()).collect();
                let prev: Vec<u8> = (0..len).map(|_| byte()).collect();
                for f in Filter::ALL {
                    let mut out = vec![0; len];
                    crate::simd::with_wide_vectors(|| apply(f, &row, &prev, bpp, &mut out));
                    for i in 0..len {
                        let a = if i >= bpp { row[i - bpp] } else { 0 };
                        let c = if i >= bpp { prev[i - bpp] } else { 0 };
                        let pred = match f {
                            Filter::None => 0,
                            Filter::Sub => a,
                            Filter::Up => prev[i],
                            Filter::Average => ((a as u16 + prev[i] as u16) / 2) as u8,
                            Filter::Paeth => paeth(a, prev[i], c),
                        };
                        assert_eq!(out[i], row[i].wrapping_sub(pred), "{f:?} bpp {bpp} len {len} at {i}");
                    }
                }
            }
        }
    }
}
