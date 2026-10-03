//! The fixed tables of RFC 1951 §3.2.5 and §3.2.6.

/// Base match length of length codes 257..=285.
pub const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
/// Extra bits of length codes 257..=285.
pub const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// Base distance of distance codes 0..=29.
pub const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
/// Extra bits of distance codes 0..=29.
pub const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// The order in which a dynamic block sends its code length code lengths.
pub const CLEN_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// Code lengths of the fixed literal/length code (288 symbols).
pub fn fixed_litlen_lengths() -> [u8; 288] {
    let mut l = [0u8; 288];
    for (i, v) in l.iter_mut().enumerate() {
        *v = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    l
}

/// Code lengths of the fixed distance code (32 symbols, all five bits).
pub const FIXED_DIST_LENGTHS: [u8; 32] = [5; 32];

const fn build_len_codes() -> [u8; 259] {
    let mut t = [0u8; 259];
    let mut len = 3;
    while len <= 258 {
        let mut c = 27;
        while LENGTH_BASE[c] as usize > len {
            c -= 1;
        }
        // 258 is also reachable as 227 + 31 (code 27), but has its own code.
        t[len] = if len == 258 { 28 } else { c as u8 };
        len += 1;
    }
    t
}

const fn search_dist(dist: usize) -> u8 {
    let mut c = 29;
    while DIST_BASE[c] as usize > dist {
        c -= 1;
    }
    c as u8
}

const fn build_dist_small() -> [u8; 257] {
    let mut t = [0u8; 257];
    let mut d = 1;
    while d <= 256 {
        t[d] = search_dist(d);
        d += 1;
    }
    t
}

/// Codes 16 and up have seven or more extra bits and bases of the form
/// 128k + 1, so (d - 1) >> 7 picks the code for every d > 256.
const fn build_dist_large() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut k = 2;
    while k < 256 {
        t[k] = search_dist(k * 128 + 1);
        k += 1;
    }
    t
}

static LEN_CODES: [u8; 259] = build_len_codes();
static DIST_SMALL: [u8; 257] = build_dist_small();
static DIST_LARGE: [u8; 256] = build_dist_large();

/// The length code (0-based, i.e. symbol - 257) for a match length 3..=258.
#[inline]
pub fn length_code(len: usize) -> usize {
    LEN_CODES[len] as usize
}

/// The distance code for a distance 1..=32768.
#[inline]
pub fn dist_code(dist: usize) -> usize {
    if dist <= 256 { DIST_SMALL[dist] as usize } else { DIST_LARGE[(dist - 1) >> 7] as usize }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_ranges_tile() {
        for len in 3..=258usize {
            let c = length_code(len);
            let base = LENGTH_BASE[c] as usize;
            assert!(len >= base && len - base < (1 << LENGTH_EXTRA[c]), "length {len}");
        }
        assert_eq!(length_code(258), 28);
        assert_eq!(length_code(257), 27);
        for d in 1..=32768usize {
            let c = dist_code(d);
            let base = DIST_BASE[c] as usize;
            assert!(d >= base && d - base < (1 << DIST_EXTRA[c]), "distance {d}");
        }
    }
}
