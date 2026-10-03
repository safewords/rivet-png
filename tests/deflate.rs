//! DEFLATE and zlib: round trips over varied data at every level and block
//! type, hand-built streams for the format's edge cases, malformed and
//! truncated streams, and a compression comparison across levels.

use rpng::deflate::{self, BlockType, Inflater, Options};

/// xorshift64*: deterministic data without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() >> 56) as u8).collect()
    }
}

fn text(n: usize) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "portable", "network", "graphics",
        "deflate", "stream", "huffman", "literal", "distance", "window", "block", "of", "and", "a", "image",
    ];
    let mut r = Rng(7);
    let mut out = Vec::with_capacity(n + 16);
    while out.len() < n {
        let w = WORDS[(r.next() % WORDS.len() as u64) as usize];
        out.extend_from_slice(w.as_bytes());
        out.push(if r.next() % 11 == 0 { b'\n' } else { b' ' });
    }
    out.truncate(n);
    out
}

fn binary(n: usize) -> Vec<u8> {
    // Little-endian counters and a slow sine: structured, not text.
    let mut out = Vec::with_capacity(n);
    let mut i = 0u32;
    while out.len() < n {
        out.extend_from_slice(&i.to_le_bytes());
        out.push(((i as f64 / 20.0).sin() * 100.0 + 128.0) as u8);
        i += 1;
    }
    out.truncate(n);
    out
}

fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let mut r = Rng(0x1234_5678);
    let random = r.bytes(100_000);
    let mut window = r.bytes(32768);
    window.extend_from_within(..);
    vec![
        ("empty", vec![]),
        ("one byte", vec![42]),
        ("two bytes", vec![1, 2]),
        ("three equal", vec![9, 9, 9]),
        ("text", text(150_000)),
        ("binary", binary(120_000)),
        ("incompressible", random),
        ("zeros", vec![0; 300_000]),
        ("abab", b"ab".repeat(70_000)),
        ("period 258", (0..258u32).map(|i| i as u8).cycle().take(80_000).collect()),
        ("32 KiB twice", window),
        ("all bytes", (0..=255u8).collect()),
        ("stored-size boundary", r.bytes(65535 * 2 + 1)),
    ]
}

#[test]
fn round_trips_every_level_and_block_type() {
    let mut count = 0;
    for (name, data) in corpus() {
        for level in 0..=9u8 {
            for bt in [BlockType::Auto, BlockType::Stored, BlockType::Fixed, BlockType::Dynamic] {
                for block_symbols in [0, 1000] {
                    let opts = Options { level, block_type: bt, block_symbols };
                    let raw = deflate::deflate_with(&data, &opts);
                    let r = Inflater::new().inflate(&raw).unwrap_or_else(|e| panic!("{name} {opts:?}: {e}"));
                    assert!(r.data == data, "{name} {opts:?}: raw round trip differs");
                    assert_eq!(r.consumed, raw.len(), "{name} {opts:?}");
                    let z = deflate::zlib_compress_with(&data, &opts);
                    assert!(deflate::zlib_decompress(&z).unwrap() == data, "{name} {opts:?}: zlib");
                    // A forced block type is the only type in the stream.
                    let want = match (level, bt) {
                        (0, _) | (_, BlockType::Stored) => Some(0),
                        (_, BlockType::Fixed) => Some(1),
                        (_, BlockType::Dynamic) => Some(2),
                        _ => None,
                    };
                    if let Some(t) = want {
                        assert!(r.blocks[t] > 0 && r.blocks.iter().sum::<usize>() == r.blocks[t], "{name} {opts:?}: {:?}", r.blocks);
                    }
                    count += 1;
                }
            }
        }
    }
    eprintln!("{count} raw + {count} zlib round trips");
}

#[test]
fn auto_picks_each_block_type_where_it_wins() {
    // Incompressible data: stored. A short text: fixed. Long text: dynamic.
    let mut r = Rng(99);
    let random = r.bytes(50_000);
    let b = Inflater::new().inflate(&deflate::deflate(&random, 6)).unwrap().blocks;
    assert!(b[0] > 0 && b[1] == 0 && b[2] == 0, "random: {b:?}");
    let b = Inflater::new().inflate(&deflate::deflate(b"hello hello", 6)).unwrap().blocks;
    assert_eq!(b, [0, 1, 0], "short text");
    let b = Inflater::new().inflate(&deflate::deflate(&text(100_000), 6)).unwrap().blocks;
    assert!(b[2] > 0 && b[0] == 0, "long text: {b:?}");
    // A mixture in one stream: text, then noise, then text, with small blocks.
    let mut mixed = text(20_000);
    mixed.extend_from_slice(&r.bytes(20_000));
    mixed.extend_from_slice(b"ab");
    let opts = Options { level: 6, block_type: BlockType::Auto, block_symbols: 4000 };
    let s = deflate::deflate_with(&mixed, &opts);
    let got = Inflater::new().inflate(&s).unwrap();
    assert_eq!(got.data, mixed);
    assert!(got.blocks[0] > 0 && got.blocks[2] > 0, "mixed: {:?}", got.blocks);
}

#[test]
fn match_at_the_full_window_distance_is_used() {
    // 32 KiB of noise twice: the second copy compresses only with matches
    // at distance exactly 32768.
    let mut r = Rng(5);
    let mut data = r.bytes(32768);
    data.extend_from_within(..);
    let c = deflate::deflate(&data, 9);
    assert!(c.len() < 32768 + 2000, "{} bytes: no 32768-distance matches", c.len());
    assert_eq!(deflate::inflate(&c).unwrap(), data);
}

#[test]
fn compression_levels_compared() {
    let sets = [("text", text(200_000)), ("binary", binary(200_000)), ("zeros", vec![0u8; 200_000])];
    eprintln!("{:<8} {}", "", (0..=9).map(|l| format!("{:>8}", format!("L{l}"))).collect::<String>());
    for (name, data) in &sets {
        let sizes: Vec<usize> = (0..=9).map(|l| deflate::zlib_compress(data, l).len()).collect();
        eprintln!("{name:<8} {}", sizes.iter().map(|s| format!("{s:>8}")).collect::<String>());
        // Stored: the data plus 5 bytes per 65535-byte block, header, trailer.
        assert_eq!(sizes[0], data.len() + 5 * data.len().div_ceil(65535) + 6);
        assert!(sizes[1] < sizes[0]);
        assert!(sizes[9] <= sizes[1], "{name}: level 9 larger than level 1");
        assert!(sizes[9] <= sizes[6] + sizes[6] / 100, "{name}: level 9 much larger than level 6");
    }
    let t = &sets[0].1;
    assert!(deflate::zlib_compress(t, 6).len() * 3 < t.len(), "text compresses less than 3:1");
}

/// A bit writer for hand-built streams (LSB first, Huffman codes MSB
/// first, as RFC 1951 §3.1.1 packs them).
#[derive(Default)]
struct Bw {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}
impl Bw {
    fn bits(&mut self, v: u32, n: u32) -> &mut Self {
        self.acc |= (v as u64) << self.n;
        self.n += n;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
        self
    }
    fn code(&mut self, code: u32, len: u32) -> &mut Self {
        for i in (0..len).rev() {
            self.bits((code >> i) & 1, 1);
        }
        self
    }
    fn align(&mut self) -> &mut Self {
        if self.n > 0 {
            let n = 8 - self.n;
            self.bits(0, n);
        }
        self
    }
    /// A symbol of the fixed literal/length code.
    fn fixed(&mut self, sym: u32) -> &mut Self {
        match sym {
            0..=143 => self.code(0x30 + sym, 8),
            144..=255 => self.code(0x190 + sym - 144, 9),
            256..=279 => self.code(sym - 256, 7),
            _ => self.code(0xC0 + sym - 280, 8),
        }
    }
    fn stored(&mut self, last: bool, data: &[u8]) -> &mut Self {
        self.bits(last as u32, 1).bits(0, 2).align();
        let n = data.len() as u32;
        self.bits(n, 16).bits(!n & 0xFFFF, 16);
        self.out.extend_from_slice(data);
        self
    }
    fn done(&mut self) -> Vec<u8> {
        self.align();
        std::mem::take(&mut self.out)
    }
}

#[test]
fn hand_built_edge_cases() {
    // Length 258 (code 285) at distance 1: a run of 259 bytes.
    let s = Bw::default().bits(1, 1).bits(1, 2).fixed(b'a' as u32).fixed(285).code(0, 5).fixed(256).done();
    assert_eq!(deflate::inflate(&s).unwrap(), vec![b'a'; 259]);

    // Distance 32768 (code 29, 13 extra bits all ones) and length 258 after
    // a 32 KiB stored block.
    let mut r = Rng(3);
    let first = r.bytes(32768);
    let s = Bw::default()
        .stored(false, &first)
        .bits(1, 1)
        .bits(1, 2)
        .fixed(285)
        .code(29, 5)
        .bits(8191, 13)
        .fixed(256)
        .done();
    let out = deflate::inflate(&s).unwrap();
    assert_eq!(out.len(), 32768 + 258);
    assert_eq!(&out[32768..], &first[..258]);

    // One byte short of the window: the same distance is out of range.
    let s = Bw::default()
        .stored(false, &first[1..])
        .bits(1, 1)
        .bits(1, 2)
        .fixed(257)
        .code(29, 5)
        .bits(8191, 13)
        .fixed(256)
        .done();
    assert!(deflate::inflate(&s).is_err());

    // Empty blocks of all three types before a final one; the final bit
    // ends the stream and anything after it is not read.
    let mut w = Bw::default();
    w.stored(false, &[]);
    w.bits(0, 1).bits(1, 2).fixed(256); // empty fixed block
    // Empty dynamic block: literal code {0, 256} one bit each, one distance
    // code; code length code {1, 18} one bit each.
    w.bits(0, 1).bits(2, 2).bits(0, 5).bits(0, 5).bits(14, 4);
    // HCLEN order 16 17 18 0 8 7 9 6 10 5 11 4 12 3 13 2 14 1.
    for s in [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1] {
        w.bits(if s == 18 || s == 1 { 1 } else { 0 }, 3);
    }
    // Lengths: 1, 255 zeros (18: 138 then 117), 1 (symbol 256), 1 (dist 0).
    w.code(0, 1).code(1, 1).bits(138 - 11, 7).code(1, 1).bits(117 - 11, 7).code(0, 1).code(0, 1);
    w.code(1, 1); // end of block (symbol 256 has code 1)
    w.bits(1, 1).bits(1, 2).fixed(b'Z' as u32).fixed(256);
    let mut s = w.done();
    let len = s.len();
    s.extend_from_slice(b"trailing garbage");
    let r = Inflater::new().inflate(&s).unwrap();
    assert_eq!(r.data, b"Z");
    assert_eq!(r.blocks, [1, 2, 1]);
    assert_eq!(r.consumed, len);

    // A stream that never sets the final bit is truncated.
    let s = Bw::default().stored(false, b"abc").done();
    assert_eq!(deflate::inflate(&s), Err(deflate::Error::Truncated));
}

#[test]
fn malformed_streams_are_rejected() {
    let bad = |s: &[u8]| deflate::inflate(s).is_err();
    // Reserved block type.
    assert!(bad(&Bw::default().bits(1, 1).bits(3, 2).done()));
    // Stored block whose NLEN is not LEN's complement.
    assert!(bad(&Bw::default().bits(1, 1).bits(0, 2).align().bits(3, 16).bits(3, 16).done()));
    // Distance before the start of the output.
    assert!(bad(&Bw::default().bits(1, 1).bits(1, 2).fixed(b'a' as u32).fixed(257).code(1, 5).fixed(256).done()));
    // Length symbol 286 and distance symbol 30 (in the fixed code, unused).
    assert!(bad(&Bw::default().bits(1, 1).bits(1, 2).fixed(b'a' as u32).fixed(286).code(0, 5).done()));
    assert!(bad(&Bw::default().bits(1, 1).bits(1, 2).fixed(b'a' as u32).fixed(257).code(30, 5).done()));
    // HLIT of 287 (> 286).
    assert!(bad(&Bw::default().bits(1, 1).bits(2, 2).bits(30, 5).bits(0, 5).bits(0, 4).done()));
    // Over-subscribed code length code: three 1-bit codes.
    let mut w = Bw::default();
    w.bits(1, 1).bits(2, 2).bits(0, 5).bits(0, 5).bits(0, 4);
    w.bits(1, 3).bits(1, 3).bits(1, 3).bits(0, 3);
    assert!(bad(&w.done()));
    // Repeat (16) with no previous length.
    let mut w = Bw::default();
    w.bits(1, 1).bits(2, 2).bits(0, 5).bits(0, 5).bits(0, 4);
    w.bits(1, 3).bits(0, 3).bits(1, 3).bits(0, 3); // 16 and 18 get one bit each
    w.code(0, 1).bits(0, 2);
    assert!(bad(&w.done()));

    // zlib wrapper errors.
    let good = deflate::zlib_compress(b"hello zlib", 6);
    assert!(deflate::zlib_decompress(&good).is_ok());
    let mut b = good.clone();
    b[1] ^= 1; // header check
    assert!(deflate::zlib_decompress(&b).is_err());
    let mut b = good.clone();
    let n = b.len();
    b[n - 1] ^= 1; // Adler-32
    assert!(matches!(deflate::zlib_decompress(&b), Err(deflate::Error::Checksum { .. })));
    assert!(Inflater::new().check_adler(false).zlib(&b).is_ok());
    assert!(deflate::zlib_decompress(&good[..good.len() - 1]).is_err()); // short trailer
    // FDICT set (0x78 0xBB is a valid header with FDICT).
    assert_eq!(deflate::zlib_decompress(&[0x78, 0xBB, 0, 0, 0, 0]), Err(deflate::Error::Dictionary));
    // Method 7.
    assert!(deflate::zlib_decompress(&[0x77, 0x01]).is_err());
}

#[test]
fn every_truncation_fails_cleanly() {
    let data = text(5000);
    for level in [0, 1, 6, 9] {
        let z = deflate::zlib_compress(&data, level);
        for cut in 0..z.len() {
            assert!(deflate::zlib_decompress(&z[..cut]).is_err(), "level {level}, {cut} of {} bytes", z.len());
        }
    }
}

#[test]
fn random_input_never_panics() {
    let mut r = Rng(0xDEAD_BEEF);
    let mut ok = 0;
    for i in 0..20_000 {
        let n = (r.next() % 200) as usize;
        let mut s = r.bytes(n);
        // Bias towards plausible block headers.
        if let Some(b) = s.first_mut() {
            *b = (*b & !6) | (((i % 3) as u8) << 1);
        }
        if Inflater::new().limit(1 << 20).inflate(&s).is_ok() {
            ok += 1;
        }
    }
    // Flipped bits in valid streams.
    let z = deflate::deflate(&text(3000), 6);
    for bit in 0..z.len() * 8 {
        let mut b = z.clone();
        b[bit / 8] ^= 1 << (bit % 8);
        let _ = deflate::inflate(&b);
    }
    eprintln!("{ok} of 20000 random inputs happened to decode");
}

#[test]
fn output_limit_is_enforced() {
    let z = deflate::deflate(&vec![7u8; 100_000], 6);
    assert_eq!(Inflater::new().limit(99_999).inflate(&z).map(|r| r.data.len()), Err(deflate::Error::Limit(99_999)));
    assert_eq!(Inflater::new().limit(100_000).inflate(&z).unwrap().data.len(), 100_000);
}
