# rivet-png

[![CI](https://github.com/safewords/rivet-png/actions/workflows/ci.yml/badge.svg)](https://github.com/safewords/rivet-png/actions/workflows/ci.yml)

A **PNG and APNG** decoder and encoder in Rust, with **its own DEFLATE and
zlib** (inflate and deflate): no C, no system libraries, no build script,
nothing to install on a build host. Written from the W3C PNG specification
(third edition, which takes in APNG, `cICP`, `mDCV`, `cLLI` and `eXIf`) and
RFCs 1950 and 1951, not translated from any other implementation. The
decoder reads all 162 valid images of PngSuite and rejects all 14 corrupt
ones, and decodes them to exactly the pixels the suite's construction calls
for (the figures are [below](#how-it-is-checked)).

Written for the **[rivet](https://github.com/safewords/rivet)**
transcoder's still-image path, where it takes the place of the third-party
`image` / `png` crates for PNG in and PNG out. Its DEFLATE codec is a public
module, [`rpng::deflate`](src/deflate/mod.rs), for rivet's other formats
that carry zlib or raw DEFLATE streams (TIFF's Deflate compression among
them).

Published as `rivet-png`; **imported as `rpng`** (`use rpng::…`), since
`png` is already the name of a crate in rivet's dependency graph. One
dependency (`thiserror`), no build script; `unsafe` only in the vector
kernels ([`src/simd.rs`](src/simd.rs)), which the `force-scalar` feature
compiles out.

```toml
[dependencies]
rpng = { package = "rivet-png", git = "https://github.com/safewords/rivet-png", branch = "develop" }
```

## What it decodes

| | |
|---|---|
| **Colour types and depths** | all fifteen: greyscale 1, 2, 4, 8, 16; truecolour 8, 16; indexed 1, 2, 4, 8; greyscale + alpha 8, 16; truecolour + alpha 8, 16 |
| **Layout** | non-interlaced and Adam7; any number and size of IDAT chunks (including zero-length ones) |
| **Transparency** | `tRNS` for greyscale, truecolour and palettes (any number of entries up to the palette's) |
| **Colour space** | `gAMA`, `cHRM`, `sRGB`, `iCCP` (profile decompressed), `cICP`, `mDCV`, `cLLI`, `sBIT`: reported in [`Metadata`](src/types.rs), not applied to the pixels |
| **Other chunks** | `bKGD`, `pHYs`, `tIME`, `eXIf`, `tEXt`, `zTXt`, `iTXt` (Latin-1 and UTF-8, compressed or not); `hIST`, `sPLT` and unknown ancillary chunks kept as bytes |
| **APNG** | `acTL`, `fcTL`, `fdAT`: every frame decoded (the static image as frame 0 or hidden), sequence numbers checked; [`Animation::compose`](src/decode.rs) renders the canvas after each frame with its dispose (none, background, previous) and blend (source, over) operations |
| **Checks** | the signature; every chunk's CRC; the zlib Adler-32; IHDR's fields; chunk order (IHDR first, one PLTE before IDAT, consecutive IDATs, IEND); filter types; the exact amount of image data |
| **Refused** | unknown critical chunks (`Error::Unsupported`), compression and filter methods other than 0; images above a pixel limit (2^28 by default) |

Pixels come back in PNG's own layout ([`Image`](src/types.rs): packed
samples, big-endian at 16 bits, with the palette and `tRNS`), and
`Image::to_rgba8` / `to_rgba16` convert (palette applied, `tRNS` to alpha,
low depths scaled by bit replication, 16 to 8 bits rounded).

Corrupt input comes back as an `Error`, never a panic: every truncation and
every single-bit flip of a selection of PngSuite files is exercised in the
tests. A malformed APNG layer (bad sequence numbers, a frame count that
disagrees with `acTL`, a frame outside the canvas) sets the animation aside
and keeps the static image, as the APNG specification asks; `Png::animation_error`
says why.

## What it encodes

- Every colour type and bit depth, palettes (with `tRNS` alpha), greyscale
  and truecolour `tRNS`, a suggested palette for truecolour.
- Any of the five filters on every row, or **adaptive selection** per row
  (the filter whose output has the least sum of absolute values; indexed and
  sub-byte images left unfiltered, as the specification suggests, unless
  `FilterStrategy::AdaptiveAlways`).
- **Adam7** interlacing.
- **Compression levels 0-9**, and a forced block type (stored, fixed,
  dynamic) through [`deflate::Options`](src/deflate/compress.rs).
- Every metadata chunk the decoder reports (`gAMA`, `cHRM`, `sRGB`, `iCCP`,
  `cICP`, `mDCV`, `cLLI`, `sBIT`, `bKGD`, `pHYs`, `eXIf`, `tIME`, the three
  text chunks, raw ancillary chunks), in the order the specification
  requires.
- **APNG**: frames at offsets with delays and dispose / blend operations,
  the static image either the first frame or a separate one; frames are
  checked against the canvas and the static image's format.

## The DEFLATE codec

`rpng::deflate` stands on its own:

```rust
use rpng::deflate::{self, BlockType, Inflater, Options};

let z = deflate::zlib_compress(data, 6);                  // zlib (RFC 1950)
let raw = deflate::deflate(data, 9);                      // raw DEFLATE (RFC 1951)
let back = deflate::zlib_decompress(&z)?;
let back = deflate::inflate(&raw)?;

// Untrusted input: cap the output, learn where the stream ended.
let r = Inflater::new().limit(64 << 20).zlib(&z)?;        // r.data, r.consumed, r.blocks
// Force a block type, or the symbols per block.
let fixed = deflate::deflate_with(data, &Options { level: 6, block_type: BlockType::Fixed, block_symbols: 0 });
// Checksums.
let (c, a) = (deflate::crc32(data), deflate::adler32(data));
```

The compressor matches over the full 32 KiB window with hash chains, greedy
at levels 1-3 and lazy at 4-9, choosing between a longer, farther match and
a shorter, nearer one by an estimate of the bits each saves. Each block is
written stored, with the fixed code or with its own code (length-limited by
package-merge), whichever is smallest. Levels 8 and 9 also try a
short-chain parse and keep the smaller result, since long chains can find
matches that cost more than they save. The decompressor uses a 10-bit
lookup table per Huffman code with a canonical fallback for longer codes.

## How it is checked

`cargo test` runs everything below; nothing is fetched and nothing external
runs.

**PngSuite** (Willem van Schaik, 2017-07-19 release, committed in
`tests/pngsuite` with its source and the SHA-256 of every file in
`SHA256SUMS`, checked on every run):

- **162 / 162** valid images decode; **14 / 14** corrupt images (`x*.png`)
  are rejected, each for its documented reason (bad signature by byte or
  line-ending damage, colour types 1 and 9, bit depths 0, 3 and 99, no IDAT,
  bad IHDR and IDAT CRCs).
- Header fields match every file name (colour type, depth, interlacing,
  sizes 1x1 to 40x40 and the 8x32 / 32x8 / 8x8 `pHYs` images).
- **Expected pixels from the suite's construction**: 49 groups of files
  that the suite builds as one picture encoded different ways must decode
  identically (71 comparisons): interlaced against non-interlaced for all
  fifteen basic formats and the eighteen odd sizes; compression levels 0, 3,
  6 and 9; image data in 1, 2, 4 and one-byte IDAT chunks; with and without
  `bKGD`, `hIST`, `sPLT`, `tIME` and text; **palette against truecolour**
  for the same colours; palette `tRNS` under four backgrounds. The five
  filter files (`f00`–`f04`, one filter type each) agree everywhere outside
  the digit each draws.
- The one file written with stored (uncompressed) blocks, `z00n2c08`, is
  decoded a second time by a minimal decoder inside the test that shares no
  code with the crate, and must match.
- Metadata against the suite's descriptions: six gamma values, `bKGD`
  colours, three `tIME` dates, four `pHYs`, `sBIT`, `cHRM`, `eXIf`, `tEXt`,
  `zTXt`, and `iTXt` in English, Finnish, Greek, Hindi and Japanese.
  Transparency: no alpha where there is no `tRNS`, exactly 0 and opaque where
  there is, alpha 0 / 85 / 170 / 255 for `tm3n3p02`; `sBIT` images' samples
  are consistent with their significant bits.
- Every valid image re-encoded with each of the five filters, adaptive and
  adaptive-always, with and without Adam7 (2268 encodes), decodes to the same
  image and metadata.
- Every truncation and every single-bit flip of six files (58,500 cases):
  an error or a decode, never a panic.

**DEFLATE / zlib** (`tests/deflate.rs`):

- Round trips of thirteen inputs (empty, 1-3 bytes, 150 KB of text, binary
  records, 100 KB of noise, 300 KB of zeros, `abab…`, a 258-byte period,
  32 KiB of noise twice, all byte values, 128 KiB + 1 of noise across the
  stored-block size) at every level 0-9, each block type forced and chosen,
  default and 1000-symbol blocks: **1040 raw and 1040 zlib round trips**,
  exact. A forced block type is checked to be the only one in the stream;
  the automatic choice is checked to pick stored for noise, fixed for a few
  bytes and dynamic for text, and to mix types within one stream.
- Hand-built streams for the edge cases: length 258 (code 285); distance
  32768 after a 32 KiB stored block (and the same distance one byte too
  early, rejected); empty stored, fixed and dynamic blocks; data after the
  final block ignored and `consumed` exact; a stream without a final block.
- Rejected: block type 3, a stored block's bad NLEN, a distance before the
  start, length symbols 286-287 and distance symbols 30-31, HLIT above 286,
  an over-subscribed code, a repeat with nothing to repeat, and in zlib a
  bad header check, FDICT, a wrong Adler-32, a short trailer, method 7.
  Every truncation of four streams fails; 20,000 random inputs and every
  single-bit flip of a stream never panic; the output limit holds.
- 32 KiB of noise twice compresses to under 34 KB, so matches at the full
  32768 distance are found and used.

Compression by level (zlib bytes; `cargo test --test deflate -- --nocapture`):

| 200 KB of | L0 | L1 | L2 | L3 | L4 | L5 | L6 | L7 | L8 | L9 |
|---|---|---|---|---|---|---|---|---|---|---|
| text | 200026 | 50349 | 44462 | 42019 | 41958 | 39030 | 36007 | 34990 | 34983 | 34983 |
| binary records | 200026 | 102740 | 102733 | 102730 | 102730 | 103095 | 104175 | 106233 | 102730 | 102730 |
| zeros | 200026 | 216 | 216 | 216 | 216 | 216 | 216 | 216 | 216 | 216 |

**Encoder** (`tests/encode.rs`): all fifteen formats at five sizes (1x1 to
33x17, including 64x1 and 1x9), with palettes and `tRNS`, under seven filter
strategies, with and without Adam7, at levels 0, 1, 6 and 9: **4200 exact
round trips**. The filter bytes written are the ones asked for; adaptive
selection filters a smooth image and comes out smaller than no filtering;
the interlaced stream has exactly the seven passes' rows. Every metadata
chunk round-trips. APNG: four frames exercising every dispose and blend
operation round-trip and compose to hand-computed canvases, interlaced or
not, with fdAT split into 7-byte chunks; indexed frames; a hidden static
image; frames that do not fit are refused. The decoder's chunk rules are
checked on hand-built files.

Speed, for scale (release build, Ryzen 9 9950X, three 1080x720 RGB frames
of camera video, 2.3 megapixels in all): decode about 95 megapixels a
second; encode about 100 at level 1, 39 at level 6 and 12 at level 9. The
encoder filters rows and matches its input in 256 KiB segments (each also
searching the 32 KiB before it) on as many threads as the machine has
(`deflate::Options::threads`); the output is the same on any number of
threads. CRC-32 is computed by carry-less multiplication (PCLMULQDQ,
PMULL), Adler-32 32 or 16 bytes at a time (AVX2, NEON), and the reverse
filters a pixel at a time in vector lanes, all chosen at run time and
checked against the portable code they replace.

## Where the specifications leave room

- **Excess image data** (the zlib stream inflates to more than the image
  needs) is an error, as is too little; data after IEND is ignored.
- **A CRC error in any chunk**, ancillary included, fails the decode
  (`Decoder::check_crc(false)` turns CRC and Adler-32 checks off). A
  malformed ancillary chunk with a good CRC (wrong length, text without a
  keyword, an undecompressable `zTXt`) is skipped.
- **A palette index past the end of the palette** decodes as opaque black.
  A `PLTE` with more entries than the bit depth can index is accepted when
  decoding and refused when encoding.
- `tRNS` after IDAT or before PLTE is ignored.
- **Length 258 written as code 284 with extra bits 31** is accepted by the
  inflater (RFC 1951's table gives code 284 the range 227-257); this encoder
  never writes it. Incomplete Huffman codes are accepted; using one of their
  missing codes is an error.
- APNG **blend over** is computed in floating point on 16-bit values and
  rounded; the specification gives the formula, not a rounding.
- Colour-space chunks are reported, not applied; converting to a display
  space is the caller's (rivet's) business.

## Provenance and licensing

Written from the W3C PNG specification (third edition), RFC 1950 and
RFC 1951; **no PNG or DEFLATE implementation's source was read** — not
libpng, zlib, zlib-ng, miniz, libdeflate, stb_image, lodepng, or the png,
flate2, miniz_oxide or fdeflate crates. No other implementation is used in
the tests either: expected results come from PngSuite's construction, the
RFCs and hand-built streams. See [NOTICE](NOTICE).

## Using it

```rust
// Decode: the static image, metadata and any animation.
let png = rpng::decode(&bytes)?;
let rgba: Vec<u8> = png.image.to_rgba8();
if let Some(anim) = &png.animation {
    for frame in anim.compose(png.image.width, png.image.height) {
        // frame.rgba16, frame.delay_num / frame.delay_den
    }
}

// Encode: level 9, adaptive filters, interlaced, with an sRGB chunk.
use rpng::{Encoder, Image, FilterStrategy};
let img = Image::from_rgba8(width, height, rgba)?;
let mut enc = Encoder { interlaced: true, filter: FilterStrategy::Adaptive, ..Encoder::with_level(9) };
enc.metadata.srgb = Some(0);
let bytes = enc.encode(&img)?;

// APNG.
use rpng::{Animation, Frame, FrameControl};
let anim = Animation {
    num_plays: 0,
    default_image_is_first_frame: true,
    frames: vec![Frame { control: FrameControl::full(width, height), image: img }],
};
let apng = Encoder::default().encode_animation(&anim, None)?;
```

## License

Open Encoding Attribution License v1.0 — a source-available (not OSI open-source)
license, royalty-free, with a commercial-attribution requirement. See
[LICENSE.md](LICENSE.md) and [NOTICE](NOTICE).
