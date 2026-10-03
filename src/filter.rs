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
        Filter::Sub => {
            for i in bpp..row.len() {
                row[i] = row[i].wrapping_add(row[i - bpp]);
            }
        }
        Filter::Up => {
            for (x, &b) in row.iter_mut().zip(prev) {
                *x = x.wrapping_add(b);
            }
        }
        Filter::Average => {
            for i in 0..row.len() {
                let a = if i >= bpp { row[i - bpp] } else { 0 };
                row[i] = row[i].wrapping_add(((a as u16 + prev[i] as u16) / 2) as u8);
            }
        }
        Filter::Paeth => {
            for i in 0..row.len() {
                let (a, c) = if i >= bpp { (row[i - bpp], prev[i - bpp]) } else { (0, 0) };
                row[i] = row[i].wrapping_add(paeth(a, prev[i], c));
            }
        }
    }
}

/// Applies `filter` to `row` (previous row `prev`), writing to `out`.
pub(crate) fn apply(filter: Filter, row: &[u8], prev: &[u8], bpp: usize, out: &mut [u8]) {
    for i in 0..row.len() {
        let a = if i >= bpp { row[i - bpp] } else { 0 };
        let b = prev[i];
        let c = if i >= bpp { prev[i - bpp] } else { 0 };
        let pred = match filter {
            Filter::None => 0,
            Filter::Sub => a,
            Filter::Up => b,
            Filter::Average => ((a as u16 + b as u16) / 2) as u8,
            Filter::Paeth => paeth(a, b, c),
        };
        out[i] = row[i].wrapping_sub(pred);
    }
}

fn cost(bytes: &[u8]) -> u64 {
    bytes.iter().map(|&b| (b as i8).unsigned_abs() as u64).sum()
}

/// Filters `rows` (each `row_len` bytes) into `out`, with a filter type byte
/// before each row.
pub(crate) fn filter_rows(
    rows: &[u8],
    row_len: usize,
    bpp: usize,
    strategy: FilterStrategy,
    low_depth_or_indexed: bool,
    out: &mut Vec<u8>,
) {
    if row_len == 0 {
        return;
    }
    let zero = vec![0u8; row_len];
    let mut trial = vec![0u8; row_len];
    let mut best = vec![0u8; row_len];
    for (y, row) in rows.chunks_exact(row_len).enumerate() {
        let prev = if y == 0 { &zero[..] } else { &rows[(y - 1) * row_len..y * row_len] };
        let choice = match strategy {
            FilterStrategy::Fixed(f) => {
                apply(f, row, prev, bpp, &mut best);
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
                    apply(f, row, prev, bpp, &mut trial);
                    let c = cost(&trial);
                    if c < chosen_cost {
                        chosen_cost = c;
                        chosen = f;
                        std::mem::swap(&mut trial, &mut best);
                    }
                }
                chosen
            }
        };
        out.push(choice.code());
        out.extend_from_slice(&best);
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
}
