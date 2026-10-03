//! Canonical Huffman codes (RFC 1951 §3.2.2): decoding tables built from
//! code lengths, length-limited code lengths built from symbol counts
//! (package-merge), and the codes themselves.

use super::Error;

/// The longest code DEFLATE allows.
pub const MAX_BITS: usize = 15;
/// Codes this long or shorter decode with one table lookup.
const FAST_BITS: u32 = 10;

/// A decoding table for one canonical code.
///
/// Codes of up to [`FAST_BITS`] bits are found by indexing `fast` with the
/// next bits of the stream (DEFLATE sends Huffman codes most significant bit
/// first, so the table is indexed by the bit-reversed code). Longer codes are
/// decoded canonically from `counts` and `symbols`: the codes of each length
/// are consecutive integers, starting where the previous length's codes
/// ended, doubled.
pub struct Decoder {
    /// `symbol << 4 | length`, or 0 where no code of <= FAST_BITS bits fits.
    fast: Vec<u16>,
    counts: [u16; MAX_BITS + 1],
    /// Symbols ordered by (length, symbol value).
    symbols: Vec<u16>,
}

impl Decoder {
    /// Builds the decoder for `lengths` (0 = symbol unused). Rejects an
    /// over-subscribed code; an incomplete one is accepted, and a stream that
    /// uses one of its missing codes fails when decoded.
    pub fn new(lengths: &[u8]) -> Result<Self, Error> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut left: i32 = 1;
        for &c in &counts[1..] {
            left <<= 1;
            left -= c as i32;
            if left < 0 {
                return Err(Error::Invalid("over-subscribed Huffman code"));
            }
        }
        let mut offsets = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0u16; offsets[MAX_BITS + 1] as usize];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offsets[l as usize] as usize] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        let mut fast = vec![0u16; 1 << FAST_BITS];
        let mut code: u32 = 0;
        let mut index = 0usize;
        for len in 1..=MAX_BITS as u32 {
            for _ in 0..counts[len as usize] {
                if len <= FAST_BITS {
                    let rev = reverse(code, len);
                    let entry = (symbols[index] << 4) | len as u16;
                    let mut i = rev as usize;
                    while i < fast.len() {
                        fast[i] = entry;
                        i += 1 << len;
                    }
                }
                code += 1;
                index += 1;
            }
            code <<= 1;
        }
        Ok(Decoder { fast, counts, symbols })
    }

    /// Decodes one symbol from `bits` (the next bits of the stream, first
    /// bit in bit 0; at least 15 valid or zero-padded). Returns the symbol
    /// and the code length.
    #[inline]
    pub fn decode(&self, bits: u32) -> Result<(u16, u32), Error> {
        let e = self.fast[(bits & ((1 << FAST_BITS) - 1)) as usize];
        if e != 0 {
            return Ok((e >> 4, (e & 15) as u32));
        }
        self.decode_slow(bits)
    }

    fn decode_slow(&self, bits: u32) -> Result<(u16, u32), Error> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=MAX_BITS {
            code |= ((bits >> (len - 1)) & 1) as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok((self.symbols[(index + code - first) as usize], len as u32));
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(Error::Invalid("a Huffman code that the block's code does not define"))
    }
}

/// `code`'s low `len` bits, reversed.
#[inline]
pub fn reverse(code: u32, len: u32) -> u32 {
    code.reverse_bits() >> (32 - len)
}

/// The canonical codes for `lengths`, already bit-reversed for an LSB-first
/// writer.
pub fn codes(lengths: &[u8]) -> Vec<u16> {
    let mut counts = [0u32; MAX_BITS + 1];
    for &l in lengths {
        counts[l as usize] += 1;
    }
    counts[0] = 0;
    let mut next = [0u32; MAX_BITS + 2];
    let mut code = 0;
    for bits in 1..=MAX_BITS {
        code = (code + counts[bits - 1]) << 1;
        next[bits] = code;
    }
    lengths
        .iter()
        .map(|&l| {
            if l == 0 {
                0
            } else {
                let c = next[l as usize];
                next[l as usize] += 1;
                reverse(c, l as u32) as u16
            }
        })
        .collect()
}

/// Optimal code lengths of at most `limit` bits for symbol counts `freqs`,
/// by package-merge. Every symbol with a non-zero count gets a code; if
/// fewer than two do, symbols are added so that the code is complete (a
/// lone code is given one bit, as RFC 1951 asks, and a partner).
pub fn lengths(freqs: &[u32], limit: usize) -> Vec<u8> {
    let mut out = vec![0u8; freqs.len()];
    let mut leaves: Vec<(u64, usize)> =
        freqs.iter().enumerate().filter(|&(_, &f)| f > 0).map(|(s, &f)| (f as u64, s)).collect();
    // Pad to two symbols so every tree has a root with two children.
    let mut s = 0;
    while leaves.len() < 2 && s < freqs.len() {
        if !leaves.iter().any(|&(_, x)| x == s) {
            leaves.push((1, s));
        }
        s += 1;
    }
    if leaves.len() < 2 {
        // A one-symbol alphabet: the caller never builds one.
        if let Some(&(_, s)) = leaves.first() {
            out[s] = 1;
        }
        return out;
    }
    leaves.sort();
    let n = leaves.len();
    debug_assert!(n <= 1 << limit);

    // An item is a leaf (index into `leaves`) or a package of two items of
    // the previous (deeper) list.
    #[derive(Clone, Copy)]
    enum Item {
        Leaf(usize),
        Pack(usize, usize),
    }
    let mut levels: Vec<Vec<(u64, Item)>> = Vec::with_capacity(limit);
    let base: Vec<(u64, Item)> = leaves.iter().enumerate().map(|(i, &(w, _))| (w, Item::Leaf(i))).collect();
    levels.push(base.clone());
    for _ in 1..limit {
        let prev = levels.last().unwrap();
        let mut packs = Vec::with_capacity(prev.len() / 2);
        let mut i = 0;
        while i + 1 < prev.len() {
            packs.push((prev[i].0 + prev[i + 1].0, Item::Pack(i, i + 1)));
            i += 2;
        }
        // Merge leaves and packages by weight (leaves first on ties).
        let mut merged = Vec::with_capacity(base.len() + packs.len());
        let (mut a, mut b) = (0, 0);
        while a < base.len() || b < packs.len() {
            if b >= packs.len() || (a < base.len() && base[a].0 <= packs[b].0) {
                merged.push(base[a]);
                a += 1;
            } else {
                merged.push(packs[b]);
                b += 1;
            }
        }
        levels.push(merged);
    }
    // Select the 2n - 2 lightest items of the shallowest list; each time a
    // leaf appears among the selected items, and the packages they expand
    // into, its code grows by one bit.
    let mut depth = vec![0u8; n];
    let mut stack: Vec<(usize, usize)> = (0..2 * n - 2).map(|i| (levels.len() - 1, i)).collect();
    while let Some((lvl, i)) = stack.pop() {
        match levels[lvl][i].1 {
            Item::Leaf(l) => depth[l] += 1,
            Item::Pack(x, y) => {
                stack.push((lvl - 1, x));
                stack.push((lvl - 1, y));
            }
        }
    }
    for (i, &(_, sym)) in leaves.iter().enumerate() {
        out[sym] = depth[i];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kraft(l: &[u8]) -> f64 {
        l.iter().filter(|&&x| x > 0).map(|&x| 0.5f64.powi(x as i32)).sum()
    }

    #[test]
    fn package_merge_is_complete_and_limited() {
        // Fibonacci weights force a deep tree; the limit caps it.
        let mut f = vec![0u32; 30];
        let (mut a, mut b) = (1u32, 1u32);
        for x in f.iter_mut() {
            *x = a;
            let c = a.saturating_add(b);
            a = b;
            b = c;
        }
        for limit in [7, 9, 15] {
            let l = lengths(&f, limit);
            assert!(l.iter().all(|&x| x as usize <= limit && x > 0));
            assert!((kraft(&l) - 1.0).abs() < 1e-12);
        }
        let l = lengths(&[0, 5, 0], 15);
        assert_eq!(l, vec![1, 1, 0]);
        let l = lengths(&[10, 1, 1, 1, 1], 15);
        assert_eq!(l[0], 1);
        assert!((kraft(&l) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn rfc_example_codes() {
        // RFC 1951 §3.2.2: lengths (3,3,3,3,3,2,4,4) give codes
        // 010 011 100 101 110 00 1110 1111.
        let c = codes(&[3, 3, 3, 3, 3, 2, 4, 4]);
        let want = [0b010, 0b011, 0b100, 0b101, 0b110, 0b00, 0b1110, 0b1111];
        let lens = [3, 3, 3, 3, 3, 2, 4, 4];
        for i in 0..8 {
            assert_eq!(c[i] as u32, reverse(want[i], lens[i]));
        }
    }

    #[test]
    fn decoder_rejects_oversubscribed() {
        assert!(Decoder::new(&[1, 1, 1]).is_err());
        assert!(Decoder::new(&[1, 2]).is_ok()); // incomplete is accepted
    }
}
