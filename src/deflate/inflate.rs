//! Decompression: RFC 1951 blocks and the RFC 1950 wrapper.

use super::Error;
use super::checksum::adler32;
use super::huffman::Decoder;
use super::tables::{
    CLEN_ORDER, DIST_BASE, DIST_EXTRA, FIXED_DIST_LENGTHS, LENGTH_BASE, LENGTH_EXTRA,
    fixed_litlen_lengths,
};

/// An LSB-first bit reader over a byte slice (RFC 1951 §3.1.1: data
/// elements are packed starting with the least significant bit).
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, buf: 0, count: 0 }
    }

    #[inline]
    fn refill(&mut self) {
        while self.count <= 56 {
            match self.data.get(self.pos) {
                Some(&b) => {
                    self.buf |= (b as u64) << self.count;
                    self.pos += 1;
                    self.count += 8;
                }
                None => break,
            }
        }
    }

    /// Tops the buffer up to at least 56 bits with one eight-byte load, when
    /// eight bytes of input remain; returns false (doing nothing) otherwise.
    /// It keeps the whole bytes that fit; the bits loaded above `count` are
    /// the stream's next bits, which later loads write again unchanged.
    #[inline(always)]
    fn refill_fast(&mut self) -> bool {
        match self.data.get(self.pos..self.pos + 8) {
            Some(w) => {
                self.buf |= u64::from_le_bytes(w.try_into().unwrap()) << self.count;
                let take = (63 - self.count) / 8;
                self.pos += take as usize;
                self.count += take * 8;
                true
            }
            None => false,
        }
    }

    /// Drops `n` bits known to be buffered.
    #[inline(always)]
    fn skip(&mut self, n: u32) {
        debug_assert!(n <= self.count);
        self.buf >>= n;
        self.count -= n;
    }

    /// The next `n` (<= 32) bits without consuming them; bits past the end
    /// of the input read as zero.
    #[inline]
    fn peek(&mut self, n: u32) -> u32 {
        if self.count < n {
            self.refill();
        }
        (self.buf & ((1u64 << n) - 1)) as u32
    }

    #[inline]
    fn consume(&mut self, n: u32) -> Result<(), Error> {
        if n > self.count {
            return Err(Error::Truncated);
        }
        self.buf >>= n;
        self.count -= n;
        Ok(())
    }

    #[inline]
    fn bits(&mut self, n: u32) -> Result<u32, Error> {
        if n == 0 {
            return Ok(0);
        }
        let v = self.peek(n);
        self.consume(n)?;
        Ok(v)
    }

    #[inline]
    fn symbol(&mut self, d: &Decoder) -> Result<u16, Error> {
        let bits = self.peek(15);
        let (sym, len) = d.decode(bits)?;
        self.consume(len)?;
        Ok(sym)
    }

    /// Drops the bits up to the next byte boundary.
    fn align(&mut self) {
        let r = self.count % 8;
        self.buf >>= r;
        self.count -= r;
    }

    /// Bytes consumed from the input, counting any partial byte as consumed.
    fn bytes_used(&self) -> usize {
        self.pos - (self.count / 8) as usize
    }

    /// Reads `n` aligned bytes (after [`align`](Self::align)).
    fn take_bytes(&mut self, n: usize, out: &mut Vec<u8>) -> Result<(), Error> {
        debug_assert!(self.count.is_multiple_of(8));
        let mut n = n;
        while n > 0 && self.count > 0 {
            out.push(self.buf as u8);
            self.buf >>= 8;
            self.count -= 8;
            n -= 1;
        }
        // Drop the look-ahead bits above `count` (any bytes left in the
        // buffer are read again): from here the input is read directly, and
        // the next refill starts afresh.
        self.pos -= (self.count / 8) as usize;
        self.buf = 0;
        self.count = 0;
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        let slice = self.data.get(self.pos..end).ok_or(Error::Truncated)?;
        out.extend_from_slice(slice);
        self.pos = end;
        Ok(())
    }
}

/// The result of [`Inflater::inflate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inflated {
    /// The decompressed bytes.
    pub data: Vec<u8>,
    /// How many bytes of the input the stream occupied (for zlib, including
    /// the header and the Adler-32 trailer); anything after is not part of
    /// it.
    pub consumed: usize,
    /// How many blocks of each type the stream had: stored, fixed Huffman,
    /// dynamic Huffman.
    pub blocks: [usize; 3],
}

/// A configurable decompressor: an output limit and whether a zlib trailer's
/// Adler-32 is checked.
#[derive(Debug, Clone, Copy)]
pub struct Inflater {
    limit: usize,
    check_adler: bool,
    size_hint: usize,
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Inflater {
    /// No output limit; Adler-32 checked.
    pub fn new() -> Self {
        Inflater { limit: usize::MAX, check_adler: true, size_hint: 0 }
    }

    /// Fails with [`Error::Limit`] rather than produce more than `limit`
    /// bytes.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Whether a zlib stream's Adler-32 trailer is checked (default true).
    pub fn check_adler(mut self, check: bool) -> Self {
        self.check_adler = check;
        self
    }

    /// The expected output size, to allocate once.
    pub fn size_hint(mut self, bytes: usize) -> Self {
        self.size_hint = bytes;
        self
    }

    /// Decompresses a raw DEFLATE stream from the start of `input`.
    pub fn inflate(&self, input: &[u8]) -> Result<Inflated, Error> {
        let mut out = Vec::with_capacity(self.size_hint.min(self.limit).min(1 << 28));
        let mut br = Bits::new(input);
        let blocks = inflate_blocks(&mut br, &mut out, self.limit)?;
        Ok(Inflated { data: out, consumed: br.bytes_used(), blocks })
    }

    /// Decompresses a zlib stream (RFC 1950) from the start of `input`.
    pub fn zlib(&self, input: &[u8]) -> Result<Inflated, Error> {
        if input.len() < 2 {
            return Err(Error::Truncated);
        }
        let (cmf, flg) = (input[0], input[1]);
        if cmf & 0x0F != 8 {
            return Err(Error::Invalid("zlib compression method is not 8 (deflate)"));
        }
        if cmf >> 4 > 7 {
            return Err(Error::Invalid("zlib window size above 32 KiB"));
        }
        if !((cmf as u16) << 8 | flg as u16).is_multiple_of(31) {
            return Err(Error::Invalid("zlib header check bits are wrong"));
        }
        if flg & 0x20 != 0 {
            return Err(Error::Dictionary);
        }
        let mut out = Vec::with_capacity(self.size_hint.min(self.limit).min(1 << 28));
        let mut br = Bits::new(&input[2..]);
        let blocks = inflate_blocks(&mut br, &mut out, self.limit)?;
        br.align();
        let at = 2 + br.bytes_used();
        let trailer = input.get(at..at + 4).ok_or(Error::Truncated)?;
        let expected = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        if self.check_adler {
            let actual = adler32(&out);
            if actual != expected {
                return Err(Error::Checksum { expected, actual });
            }
        }
        Ok(Inflated { data: out, consumed: at + 4, blocks })
    }
}

/// Decompresses a raw DEFLATE stream (RFC 1951). Bytes after the final
/// block are ignored.
pub fn inflate(input: &[u8]) -> Result<Vec<u8>, Error> {
    Inflater::new().inflate(input).map(|r| r.data)
}

/// Decompresses a zlib stream (RFC 1950), checking its Adler-32. Bytes after
/// the trailer are ignored.
pub fn zlib_decompress(input: &[u8]) -> Result<Vec<u8>, Error> {
    Inflater::new().zlib(input).map(|r| r.data)
}

fn inflate_blocks(br: &mut Bits, out: &mut Vec<u8>, limit: usize) -> Result<[usize; 3], Error> {
    let mut fixed: Option<(Decoder, Decoder)> = None;
    let mut blocks = [0usize; 3];
    loop {
        let last = br.bits(1)? == 1;
        let btype = br.bits(2)?;
        if btype < 3 {
            blocks[btype as usize] += 1;
        }
        match btype {
            0 => {
                br.align();
                let len = br.bits(16)?;
                let nlen = br.bits(16)?;
                if len != !nlen & 0xFFFF {
                    return Err(Error::Invalid("stored block length check (NLEN) fails"));
                }
                if out.len() + len as usize > limit {
                    return Err(Error::Limit(limit));
                }
                br.take_bytes(len as usize, out)?;
            }
            1 => {
                if fixed.is_none() {
                    fixed = Some((
                        Decoder::new(&fixed_litlen_lengths())?,
                        Decoder::new(&FIXED_DIST_LENGTHS)?,
                    ));
                }
                let (lit, dist) = fixed.as_ref().unwrap();
                codes(br, out, lit, dist, limit)?;
            }
            2 => {
                let (lit, dist) = dynamic_header(br)?;
                codes(br, out, &lit, &dist, limit)?;
            }
            _ => return Err(Error::Invalid("reserved block type 3")),
        }
        if last {
            return Ok(blocks);
        }
    }
}

fn dynamic_header(br: &mut Bits) -> Result<(Decoder, Decoder), Error> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    if hlit > 286 {
        return Err(Error::Invalid("more than 286 literal/length codes"));
    }
    if hdist > 30 {
        return Err(Error::Invalid("more than 30 distance codes"));
    }
    let mut clens = [0u8; 19];
    for &i in &CLEN_ORDER[..hclen] {
        clens[i] = br.bits(3)? as u8;
    }
    let cl = Decoder::new(&clens)?;
    let mut lengths = [0u8; 286 + 30];
    let total = hlit + hdist;
    let mut i = 0;
    while i < total {
        let sym = br.symbol(&cl)?;
        let (value, repeat) = match sym {
            0..=15 => (sym as u8, 1),
            16 => {
                if i == 0 {
                    return Err(Error::Invalid("code length repeat with no previous length"));
                }
                (lengths[i - 1], 3 + br.bits(2)? as usize)
            }
            17 => (0, 3 + br.bits(3)? as usize),
            _ => (0, 11 + br.bits(7)? as usize),
        };
        if i + repeat > total {
            return Err(Error::Invalid("code lengths overrun HLIT + HDIST"));
        }
        lengths[i..i + repeat].fill(value);
        i += repeat;
    }
    if lengths[256] == 0 {
        return Err(Error::Invalid("the block's code has no end-of-block symbol"));
    }
    let lit = Decoder::new(&lengths[..hlit])?;
    let dist = Decoder::new(&lengths[hlit..total])?;
    Ok((lit, dist))
}

fn codes(br: &mut Bits, out: &mut Vec<u8>, lit: &Decoder, dist: &Decoder, limit: usize) -> Result<(), Error> {
    // Work on a local vector (and a local bit reader): stores into the
    // output bytes could otherwise alias the vector's length behind `out`,
    // forcing it to be reloaded after every byte.
    let mut local = std::mem::take(out);
    let mut bits = Bits { data: br.data, pos: br.pos, buf: br.buf, count: br.count };
    let r = codes_local(&mut bits, &mut local, lit, dist, limit);
    *out = local;
    *br = bits;
    r
}

fn codes_local(br: &mut Bits, out: &mut Vec<u8>, lit: &Decoder, dist: &Decoder, limit: usize) -> Result<(), Error> {
    loop {
        // Away from the end of the input, one refill covers a whole
        // literal or length-distance pair (at most 15 + 5 + 15 + 13 bits),
        // so the bits are taken without further checks.
        if br.count < 48 && !br.refill_fast() {
            if !codes_one(br, out, lit, dist, limit)? {
                return Ok(());
            }
            continue;
        }
        let (sym, l) = lit.decode(br.buf as u32 & 0x7FFF)?;
        br.skip(l);
        let sym = sym as usize;
        if sym < 256 {
            if out.len() >= limit {
                return Err(Error::Limit(limit));
            }
            out.push(sym as u8);
            continue;
        }
        if sym == 256 {
            return Ok(());
        }
        let li = sym - 257;
        if li >= 29 {
            return Err(Error::Invalid("length symbol 286 or 287"));
        }
        let le = LENGTH_EXTRA[li] as u32;
        let len = LENGTH_BASE[li] as usize + (br.buf & ((1 << le) - 1)) as usize;
        br.skip(le);
        let (di, l) = dist.decode(br.buf as u32 & 0x7FFF)?;
        br.skip(l);
        let di = di as usize;
        if di >= 30 {
            return Err(Error::Invalid("distance symbol 30 or 31"));
        }
        let de = DIST_EXTRA[di] as u32;
        let d = DIST_BASE[di] as usize + (br.buf & ((1 << de) - 1)) as usize;
        br.skip(de);
        copy_match(out, d, len, limit)?;
    }
}

/// One literal or length-distance pair, with every read checked (near the
/// end of the input). Returns false at the end of the block.
fn codes_one(br: &mut Bits, out: &mut Vec<u8>, lit: &Decoder, dist: &Decoder, limit: usize) -> Result<bool, Error> {
    let sym = br.symbol(lit)? as usize;
    if sym < 256 {
        if out.len() >= limit {
            return Err(Error::Limit(limit));
        }
        out.push(sym as u8);
        return Ok(true);
    }
    if sym == 256 {
        return Ok(false);
    }
    let li = sym - 257;
    if li >= 29 {
        return Err(Error::Invalid("length symbol 286 or 287"));
    }
    let len = LENGTH_BASE[li] as usize + br.bits(LENGTH_EXTRA[li] as u32)? as usize;
    let di = br.symbol(dist)? as usize;
    if di >= 30 {
        return Err(Error::Invalid("distance symbol 30 or 31"));
    }
    let d = DIST_BASE[di] as usize + br.bits(DIST_EXTRA[di] as u32)? as usize;
    copy_match(out, d, len, limit)?;
    Ok(true)
}

/// Appends the `len` bytes that start `d` back.
#[inline(always)]
fn copy_match(out: &mut Vec<u8>, d: usize, len: usize, limit: usize) -> Result<(), Error> {
    if d > out.len() {
        return Err(Error::Invalid("a distance reaching back before the start of the data"));
    }
    if out.len() + len > limit {
        return Err(Error::Limit(limit));
    }
    let start = out.len() - d;
    if d >= len {
        out.extend_from_within(start..start + len);
    } else {
        // Overlapping copy: each byte may be one this copy just wrote.
        out.reserve(len);
        for k in 0..len {
            let b = out[start + k];
            out.push(b);
        }
    }
    Ok(())
}
