//! Compression: LZ77 matching over a 32 KiB window with hash chains, then
//! each block coded stored, with the fixed code, or with its own dynamic
//! code, whichever is smallest (or as forced by [`Options::block_type`]).

use super::checksum::adler32;
use super::huffman::{codes, lengths};
use super::tables::{
    CLEN_ORDER, DIST_BASE, DIST_EXTRA, FIXED_DIST_LENGTHS, LENGTH_BASE, LENGTH_EXTRA, dist_code,
    fixed_litlen_lengths, length_code,
};

const WINDOW: usize = 32768;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: u32 = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;

/// How blocks are coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlockType {
    /// Each block as whichever of stored, fixed and dynamic is smallest.
    #[default]
    Auto,
    /// Every block stored (no compression; what level 0 does).
    Stored,
    /// Every block with the fixed Huffman code of RFC 1951 §3.2.6.
    Fixed,
    /// Every block with its own (dynamic) Huffman code.
    Dynamic,
}

/// Compression settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// 0 (store) to 9 (slowest, smallest). Levels 1-3 match greedily with
    /// short hash chains; 4-9 use lazy matching and ever longer chains.
    pub level: u8,
    /// How blocks are coded.
    pub block_type: BlockType,
    /// The most LZ77 symbols (literals and matches) per block; 0 for the
    /// default (16384). Smaller blocks adapt their codes to the data more
    /// often at the price of more headers.
    pub block_symbols: usize,
    /// The most threads to compress with (0: as many as the machine has; 1:
    /// only the calling thread). Input is matched in independent segments
    /// of 256 KiB (each also searching the 32 KiB before it), so the output
    /// is the same whatever this is.
    pub threads: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { level: 6, block_type: BlockType::Auto, block_symbols: 0, threads: 0 }
    }
}

/// The input is matched in segments of this many bytes, independently (so
/// in parallel), each block ending at a segment boundary.
const SEGMENT: usize = 1 << 18;

/// Compresses `data` into a raw DEFLATE stream at `level` (0-9; above 9 is
/// taken as 9).
pub fn deflate(data: &[u8], level: u8) -> Vec<u8> {
    deflate_with(data, &Options { level, ..Default::default() })
}

/// Compresses `data` into a zlib stream (RFC 1950) at `level`.
pub fn zlib_compress(data: &[u8], level: u8) -> Vec<u8> {
    zlib_compress_with(data, &Options { level, ..Default::default() })
}

/// Compresses `data` into a zlib stream with `options`.
pub fn zlib_compress_with(data: &[u8], options: &Options) -> Vec<u8> {
    // CMF: method 8, a 32 KiB window (CINFO 7). FLG: FLEVEL from the level
    // (informational, RFC 1950 §2.2), FDICT 0, FCHECK making the pair a
    // multiple of 31.
    let cmf = 0x78u8;
    let flevel: u8 = match options.level.min(9) {
        0 | 1 => 0,
        2..=5 => 1,
        6 => 2,
        _ => 3,
    };
    let mut flg = flevel << 6;
    flg += (31 - ((cmf as u16) << 8 | flg as u16) % 31) as u8 % 31;
    let mut out = vec![cmf, flg];
    out.extend_from_slice(&deflate_with(data, options));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Compresses `data` into a raw DEFLATE stream with `options`.
pub fn deflate_with(data: &[u8], options: &Options) -> Vec<u8> {
    let level = options.level.min(9);
    let block_type = if level == 0 { BlockType::Stored } else { options.block_type };
    let mut w = BitWriter::with_capacity(data.len() / 2 + 64);
    if data.is_empty() {
        // One final block holding nothing.
        match block_type {
            BlockType::Stored => write_stored(&mut w, &[], true),
            BlockType::Dynamic => {
                write_huffman(&mut w, &[], &Plan::dynamic(&[]), true);
            }
            _ => write_huffman(&mut w, &[], &Plan::fixed(), true),
        }
        return w.finish();
    }
    if block_type == BlockType::Stored {
        write_stored(&mut w, data, true);
        return w.finish();
    }
    let max_symbols = if options.block_symbols == 0 { 16384 } else { options.block_symbols };
    let segments = data.len().div_ceil(SEGMENT);
    let coded = crate::par::map(segments, options.threads, |i| {
        let (start, end) = (i * SEGMENT, ((i + 1) * SEGMENT).min(data.len()));
        let last = end == data.len();
        let main = lz_blocks(data, start, end, Params::for_level(level), block_type, max_symbols, last);
        if level >= 8 {
            // Long hash chains find longer but farther matches, which can
            // cost more than they save once the distances are Huffman coded;
            // the slowest levels also try a short-chain parse and keep the
            // smaller.
            let alt = lz_blocks(data, start, end, Params::for_level(4), block_type, max_symbols, last);
            if estimate(&alt) < estimate(&main) {
                return alt;
            }
        }
        main
    });
    for block in coded.iter().flatten() {
        let raw = &data[block.raw.clone()];
        if block_type == BlockType::Auto && stored_cost(raw.len(), w.pending_bits()) < block.bits {
            write_stored(&mut w, raw, block.last);
        } else {
            w.append(&block.coded);
        }
    }
    w.finish()
}

/// One block, Huffman coded on its own (its bits do not depend on where in
/// the stream it lands, unlike a stored block's padding).
struct CodedBlock {
    /// The input it codes.
    raw: std::ops::Range<usize>,
    /// Whether it is the stream's final block.
    last: bool,
    /// Its size in bits as coded.
    bits: u64,
    coded: BitWriter,
}

/// A segment's size in bits, taking each block stored when that is smaller.
fn estimate(blocks: &[CodedBlock]) -> u64 {
    blocks.iter().map(|b| b.bits.min(stored_cost(b.raw.len(), 0))).sum()
}

/// Matches `data[start..end]` (with the 32 KiB before `start` as history)
/// and codes each block with the fixed or its own dynamic code, whichever is
/// smaller (or as forced).
fn lz_blocks(
    data: &[u8],
    start: usize,
    end: usize,
    params: Params,
    block_type: BlockType,
    max_symbols: usize,
    last_segment: bool,
) -> Vec<CodedBlock> {
    let mut matcher = Matcher::new(data, params, start, end);
    let mut tokens = Vec::with_capacity(max_symbols);
    let mut blocks = Vec::new();
    let mut block_start = start;
    loop {
        tokens.clear();
        let block_end = matcher.parse(&mut tokens, max_symbols);
        let last = block_end >= end;
        let fixed = Plan::fixed();
        let plan = match block_type {
            BlockType::Fixed => fixed,
            BlockType::Dynamic => Plan::dynamic(&tokens),
            _ => {
                let dynamic = Plan::dynamic(&tokens);
                if fixed.cost(&tokens) <= dynamic.cost(&tokens) { fixed } else { dynamic }
            }
        };
        let mut coded = BitWriter::with_capacity(tokens.len() + 64);
        write_huffman(&mut coded, &tokens, &plan, last && last_segment);
        blocks.push(CodedBlock {
            raw: block_start..block_end,
            last: last && last_segment,
            bits: coded.out.len() as u64 * 8 + coded.count as u64,
            coded,
        });
        block_start = block_end;
        if last {
            return blocks;
        }
    }
}

/// An LZ77 symbol: a literal byte, or a match of `len` bytes `dist` back.
#[derive(Clone, Copy)]
enum Token {
    Lit(u8),
    Match { len: u16, dist: u16 },
}

struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    count: u32,
}

impl BitWriter {
    fn with_capacity(n: usize) -> Self {
        BitWriter { out: Vec::with_capacity(n), buf: 0, count: 0 }
    }

    #[inline]
    fn put(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32 && (n == 32 || value >> n == 0));
        self.buf |= (value as u64) << self.count;
        self.count += n;
        while self.count >= 8 {
            self.out.push(self.buf as u8);
            self.buf >>= 8;
            self.count -= 8;
        }
    }

    fn align(&mut self) {
        if self.count > 0 {
            self.put(0, 8 - self.count);
        }
    }

    fn pending_bits(&self) -> u32 {
        self.count
    }

    /// Appends everything `other` holds, its pending bits included.
    fn append(&mut self, other: &BitWriter) {
        if self.count == 0 {
            self.out.extend_from_slice(&other.out);
        } else {
            let (shift, back) = (self.count, 8 - self.count);
            let mut carry = self.buf as u8;
            self.out.reserve(other.out.len());
            for &b in &other.out {
                self.out.push(carry | (b << shift));
                carry = b >> back;
            }
            self.buf = carry as u64;
        }
        if other.count > 0 {
            self.put(other.buf as u32, other.count);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

fn stored_cost(len: usize, pending: u32) -> u64 {
    // Per 65535-byte piece: 3 header bits padded to a byte, LEN and NLEN.
    let pieces = len.div_ceil(65535).max(1) as u64;
    let pad = (8 - (pending + 3) % 8) % 8;
    pieces * (3 + 32) + pad as u64 + 8 * len as u64 + (pieces - 1) * 7
}

fn write_stored(w: &mut BitWriter, data: &[u8], last: bool) {
    let mut pieces = data.chunks(65535).peekable();
    if data.is_empty() {
        w.put(last as u32, 1);
        w.put(0, 2);
        w.align();
        w.put(0, 16);
        w.put(0xFFFF, 16);
        return;
    }
    while let Some(piece) = pieces.next() {
        let fin = last && pieces.peek().is_none();
        w.put(fin as u32, 1);
        w.put(0, 2);
        w.align();
        let n = piece.len() as u32;
        w.put(n, 16);
        w.put(!n & 0xFFFF, 16);
        w.out.extend_from_slice(piece);
    }
}

/// The codes for one Huffman-coded block.
struct Plan {
    dynamic: bool,
    lit_len: Vec<u8>,
    dist_len: Vec<u8>,
    lit_code: Vec<u16>,
    dist_code: Vec<u16>,
}

impl Plan {
    fn fixed() -> Plan {
        let lit_len = fixed_litlen_lengths().to_vec();
        let dist_len = FIXED_DIST_LENGTHS.to_vec();
        Plan { dynamic: false, lit_code: codes(&lit_len), dist_code: codes(&dist_len), lit_len, dist_len }
    }

    fn dynamic(tokens: &[Token]) -> Plan {
        let mut lf = vec![0u32; 286];
        let mut df = vec![0u32; 30];
        for t in tokens {
            match *t {
                Token::Lit(b) => lf[b as usize] += 1,
                Token::Match { len, dist } => {
                    lf[257 + length_code(len as usize)] += 1;
                    df[dist_code(dist as usize)] += 1;
                }
            }
        }
        lf[256] = 1;
        let lit_len = lengths(&lf, 15);
        // `lengths` gives a two-code tree even for zero or one used
        // distance; strict decoders reject a lone one-bit code.
        let dist_len = lengths(&df, 15);
        Plan { dynamic: true, lit_code: codes(&lit_len), dist_code: codes(&dist_len), lit_len, dist_len }
    }

    /// The block's size in bits, header included.
    fn cost(&self, tokens: &[Token]) -> u64 {
        let mut bits = 3u64 + self.lit_len[256] as u64;
        for t in tokens {
            bits += match *t {
                Token::Lit(b) => self.lit_len[b as usize] as u64,
                Token::Match { len, dist } => {
                    let lc = length_code(len as usize);
                    let dc = dist_code(dist as usize);
                    (self.lit_len[257 + lc] + LENGTH_EXTRA[lc] + self.dist_len[dc] + DIST_EXTRA[dc]) as u64
                }
            };
        }
        if self.dynamic {
            bits += self.header().bits;
        }
        bits
    }

    fn header(&self) -> Header {
        let hlit = 257.max(self.lit_len.iter().rposition(|&l| l != 0).map_or(0, |p| p + 1));
        let hdist = 1.max(self.dist_len.iter().rposition(|&l| l != 0).map_or(0, |p| p + 1));
        let mut seq: Vec<u8> = self.lit_len[..hlit].to_vec();
        seq.extend_from_slice(&self.dist_len[..hdist]);
        // Run-length code the lengths with symbols 16 (repeat the previous
        // length 3-6 times), 17 (3-10 zeros) and 18 (11-138 zeros).
        let mut rle: Vec<(u8, u8)> = Vec::new();
        let mut i = 0;
        while i < seq.len() {
            let v = seq[i];
            let mut run = 1;
            while i + run < seq.len() && seq[i + run] == v {
                run += 1;
            }
            let mut left = run;
            if v == 0 {
                while left >= 11 {
                    let n = left.min(138);
                    rle.push((18, (n - 11) as u8));
                    left -= n;
                }
                if left >= 3 {
                    rle.push((17, (left - 3) as u8));
                    left = 0;
                }
            } else {
                rle.push((v, 0));
                left -= 1;
                while left >= 3 {
                    let n = left.min(6);
                    rle.push((16, (n - 3) as u8));
                    left -= n;
                }
            }
            for _ in 0..left {
                rle.push((v, 0));
            }
            i += run;
        }
        let mut cf = [0u32; 19];
        for &(s, _) in &rle {
            cf[s as usize] += 1;
        }
        let cl_len = lengths(&cf, 7);
        let cl_code = codes(&cl_len);
        let hclen = 4.max(CLEN_ORDER.iter().rposition(|&s| cl_len[s] != 0).map_or(0, |p| p + 1));
        let mut bits = 5 + 5 + 4 + 3 * hclen as u64;
        for &(s, _) in &rle {
            bits += cl_len[s as usize] as u64
                + match s {
                    16 => 2,
                    17 => 3,
                    18 => 7,
                    _ => 0,
                };
        }
        Header { hlit, hdist, hclen, rle, cl_len, cl_code, bits }
    }
}

struct Header {
    hlit: usize,
    hdist: usize,
    hclen: usize,
    rle: Vec<(u8, u8)>,
    cl_len: Vec<u8>,
    cl_code: Vec<u16>,
    bits: u64,
}

fn write_huffman(w: &mut BitWriter, tokens: &[Token], plan: &Plan, last: bool) {
    w.put(last as u32, 1);
    if plan.dynamic {
        w.put(2, 2);
        let h = plan.header();
        w.put((h.hlit - 257) as u32, 5);
        w.put((h.hdist - 1) as u32, 5);
        w.put((h.hclen - 4) as u32, 4);
        for &s in &CLEN_ORDER[..h.hclen] {
            w.put(h.cl_len[s] as u32, 3);
        }
        for &(s, extra) in &h.rle {
            w.put(h.cl_code[s as usize] as u32, h.cl_len[s as usize] as u32);
            match s {
                16 => w.put(extra as u32, 2),
                17 => w.put(extra as u32, 3),
                18 => w.put(extra as u32, 7),
                _ => {}
            }
        }
    } else {
        w.put(1, 2);
    }
    for t in tokens {
        match *t {
            Token::Lit(b) => w.put(plan.lit_code[b as usize] as u32, plan.lit_len[b as usize] as u32),
            Token::Match { len, dist } => {
                let lc = length_code(len as usize);
                w.put(plan.lit_code[257 + lc] as u32, plan.lit_len[257 + lc] as u32);
                w.put(len as u32 - LENGTH_BASE[lc] as u32, LENGTH_EXTRA[lc] as u32);
                let dc = dist_code(dist as usize);
                w.put(plan.dist_code[dc] as u32, plan.dist_len[dc] as u32);
                w.put(dist as u32 - DIST_BASE[dc] as u32, DIST_EXTRA[dc] as u32);
            }
        }
    }
    w.put(plan.lit_code[256] as u32, plan.lit_len[256] as u32);
}

/// Match-finder settings for a level.
#[derive(Clone, Copy)]
struct Params {
    /// Hash chain entries examined per search.
    chain: usize,
    /// Defer a match by one byte if the next position matches longer, for
    /// matches shorter than this (0: never).
    max_lazy: usize,
    /// When the match in hand is at least this long, the look-ahead search
    /// examines a quarter of the chain.
    good: usize,
    /// A match this long ends the search.
    nice: usize,
    /// Below this level, positions inside a match are not hashed when the
    /// match is longer than this (faster, misses some matches).
    insert_limit: usize,
}

impl Params {
    fn for_level(level: u8) -> Params {
        // Chosen by measurement on text, binary data and images (see the
        // compression comparison in tests/deflate.rs).
        let (chain, max_lazy, good, nice, insert_limit) = match level {
            1 => (4, 0, 0, 16, 8),
            2 => (8, 0, 0, 32, 16),
            3 => (16, 0, 0, 64, 32),
            4 => (16, 8, 8, 32, MAX_MATCH),
            5 => (32, 16, 8, 64, MAX_MATCH),
            6 => (96, 32, 16, 128, MAX_MATCH),
            7 => (256, 64, 32, MAX_MATCH, MAX_MATCH),
            8 => (1024, 128, 64, MAX_MATCH, MAX_MATCH),
            _ => (4096, MAX_MATCH, MAX_MATCH, MAX_MATCH, MAX_MATCH),
        };
        Params { chain, max_lazy, good, nice, insert_limit }
    }
}

/// Roughly the bits a match saves over coding its bytes as literals: eight
/// per byte, less a typical length code (7 bits) and distance code (5), and
/// their extra bits. Steers the choice between a longer, farther match and
/// a shorter, nearer one.
#[inline]
fn gain(len: usize, dist: usize) -> i32 {
    8 * len as i32 - 12 - LENGTH_EXTRA[length_code(len)] as i32 - DIST_EXTRA[dist_code(dist)] as i32
}

/// The length of the common prefix of `a` and `b` (equal lengths).
#[inline]
fn match_len(a: &[u8], b: &[u8]) -> usize {
    let mut l = 0;
    let n = a.len();
    while l + 8 <= n {
        let x = u64::from_le_bytes(a[l..l + 8].try_into().unwrap()) ^ u64::from_le_bytes(b[l..l + 8].try_into().unwrap());
        if x != 0 {
            return l + (x.trailing_zeros() / 8) as usize;
        }
        l += 8;
    }
    while l < n && a[l] == b[l] {
        l += 1;
    }
    l
}

struct Matcher<'a> {
    data: &'a [u8],
    params: Params,
    /// Where matching stops (matches do not reach past it).
    end: usize,
    /// Most recent position + 1 with each hash (0 = none).
    head: Vec<u32>,
    /// For position p (mod WINDOW), the previous position + 1 with the same
    /// hash.
    prev: Vec<u32>,
    /// Next position to code.
    pos: usize,
    /// Next position to insert into the hash chains.
    inserted: usize,
    /// A match found at `pos` by the lazy look-ahead, not yet emitted.
    pending: Option<(usize, usize)>,
}

impl<'a> Matcher<'a> {
    /// Matches `data[start..end]`, with the window before `start` as history.
    fn new(data: &'a [u8], params: Params, start: usize, end: usize) -> Self {
        Matcher {
            data,
            params,
            end,
            head: vec![0; HASH_SIZE],
            prev: vec![0; WINDOW],
            pos: start,
            inserted: start.saturating_sub(WINDOW),
            pending: None,
        }
    }

    #[inline]
    fn hash(&self, p: usize) -> usize {
        let d = self.data;
        let v = (d[p] as u32) << 16 | (d[p + 1] as u32) << 8 | d[p + 2] as u32;
        (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    }

    /// Adds positions up to (not including) `upto` to the hash chains.
    fn insert_upto(&mut self, upto: usize) {
        let last = self.data.len().saturating_sub(MIN_MATCH - 1);
        while self.inserted < upto.min(last) {
            let p = self.inserted;
            let h = self.hash(p);
            self.prev[p % WINDOW] = self.head[h];
            self.head[h] = p as u32 + 1;
            self.inserted += 1;
        }
        self.inserted = self.inserted.max(upto);
    }

    /// The longest match for the bytes at `p` (positions before `p` must be
    /// in the chains, `p` itself not yet).
    fn find(&self, p: usize, chain: usize) -> (usize, usize) {
        let d = self.data;
        let max = MAX_MATCH.min(self.end - p);
        if max < MIN_MATCH {
            return (0, 0);
        }
        let mut best = (0usize, 0usize);
        let mut cand = self.head[self.hash(p)] as usize;
        let mut chain = chain;
        let mut last_cand = usize::MAX;
        while cand != 0 && chain > 0 {
            let c = cand - 1;
            // Chain entries only ever point backwards; one that does not was
            // overwritten when the ring wrapped.
            if c >= last_cand || p - c > WINDOW {
                break;
            }
            last_cand = c;
            // A candidate can only beat `best` if it matches through byte
            // best.0: compare that byte, with the three before it.
            let probe = best.0.min(max - 1);
            let worth = if probe >= 3 {
                let at = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
                at(c + probe - 3) == at(p + probe - 3)
            } else {
                d[c + probe] == d[p + probe]
            };
            if worth {
                let l = match_len(&d[c..c + max], &d[p..p + max]);
                if l > best.0 && (best.0 < MIN_MATCH || gain(l, p - c) > gain(best.0, best.1)) {
                    best = (l, p - c);
                    if l >= self.params.nice || l == max {
                        break;
                    }
                }
            }
            cand = self.prev[c % WINDOW] as usize;
            chain -= 1;
        }
        // A three-byte match far back costs more than three literals.
        if best.0 == MIN_MATCH && best.1 > 4096 {
            return (0, 0);
        }
        if best.0 >= MIN_MATCH { best } else { (0, 0) }
    }

    /// Appends up to `max` tokens; returns the input position reached.
    fn parse(&mut self, tokens: &mut Vec<Token>, max: usize) -> usize {
        let n = self.end;
        while self.pos < n && tokens.len() < max {
            let p = self.pos;
            let (len, dist) = match self.pending.take() {
                Some(m) => m,
                None => {
                    self.insert_upto(p);
                    self.find(p, self.params.chain)
                }
            };
            if len < MIN_MATCH {
                tokens.push(Token::Lit(self.data[p]));
                self.pos += 1;
                continue;
            }
            if len < self.params.max_lazy && len < self.params.nice && p + 1 < n {
                self.insert_upto(p + 1);
                let chain = if len >= self.params.good { self.params.chain / 4 } else { self.params.chain };
                let next = self.find(p + 1, chain.max(1));
                if next.0 > len && gain(next.0, next.1) > gain(len, dist) {
                    tokens.push(Token::Lit(self.data[p]));
                    self.pos += 1;
                    self.pending = Some(next);
                    continue;
                }
            }
            tokens.push(Token::Match { len: len as u16, dist: dist as u16 });
            self.pos += len;
            if len > self.params.insert_limit {
                // Skip hashing the inside of a long match.
                self.inserted = self.inserted.max(self.pos.saturating_sub(MIN_MATCH));
            }
        }
        self.pos
    }
}
