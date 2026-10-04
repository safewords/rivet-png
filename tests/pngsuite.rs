//! PngSuite (Willem van Schaik, 2017-07-19 release), as data: every valid
//! image decodes to what the suite's documented structure says it holds,
//! every x*.png is rejected. Source and SHA-256 of each file:
//! tests/pngsuite/SHA256SUMS.
//!
//! Expected pixels come from the suite's construction, not from a second
//! implementation: images the suite builds as the same picture in different
//! encodings (interlaced and not, compression levels 0-9, IDAT split four
//! ways, palette vs truecolour, with and without bKGD / hIST / sPLT / tIME /
//! text chunks, the five filter types) must decode identically; header
//! fields and metadata must match the file names and the suite's
//! descriptions; and the one stored-block, uncompressed file is decoded
//! again by a minimal decoder written in this test. Each image is also
//! re-encoded with every filter, with and without Adam7, and must decode to
//! the same image.

mod common;

use common::{hex, sha256};
use rpng::{Background, Encoder, Filter, FilterStrategy, Png, TextKind, Transparency};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("pngsuite")
}

/// (name, bytes) of every file in the manifest, hashes checked.
fn suite() -> Vec<(String, Vec<u8>)> {
    let manifest =
        std::fs::read_to_string(dir().join("SHA256SUMS")).expect("tests/pngsuite/SHA256SUMS");
    let mut out = Vec::new();
    for line in manifest
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let (hash, file) = line.split_once("  ").expect("manifest line");
        let bytes = std::fs::read(dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(
            hex(&sha256(&bytes)),
            hash,
            "{file}: SHA-256 differs from the manifest"
        );
        out.push((file.trim_end_matches(".png").to_string(), bytes));
    }
    out
}

fn decoded() -> BTreeMap<String, Png> {
    suite()
        .into_iter()
        .filter(|(n, _)| !n.starts_with('x'))
        .map(|(n, b)| {
            let png = rpng::decode(&b).unwrap_or_else(|e| panic!("{n}: {e}"));
            (n, png)
        })
        .collect()
}

#[test]
fn sha256_known_answers() {
    assert_eq!(
        hex(&sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn every_valid_image_decodes_and_every_corrupt_one_is_rejected() {
    let all = suite();
    assert_eq!(all.len(), 176, "the 2017-07-19 PngSuite has 176 PNG files");
    let (mut good, mut bad, mut rejected) = (0, 0, 0);
    for (name, bytes) in &all {
        let r = rpng::decode(bytes);
        if name.starts_with('x') {
            bad += 1;
            match r {
                Err(e) => {
                    rejected += 1;
                    eprintln!("{name}: rejected: {e}");
                }
                Ok(_) => panic!("{name}: a corrupt file decoded"),
            }
        } else {
            r.unwrap_or_else(|e| panic!("{name}: {e}"));
            good += 1;
        }
    }
    eprintln!(
        "PngSuite: {good}/{} valid images decoded, {rejected}/{bad} corrupt images rejected",
        all.len() - bad
    );
    assert_eq!((good, bad, rejected), (162, 14, 14));
}

#[test]
fn corrupt_files_fail_for_the_documented_reason() {
    let all: BTreeMap<_, _> = suite().into_iter().collect();
    let err = |n: &str| rpng::decode(&all[n]).unwrap_err().to_string();
    for n in [
        "xs1n0g01", "xs2n0g01", "xs4n0g01", "xs7n0g01", "xcrn0g04", "xlfn0g04",
    ] {
        assert!(err(n).contains("signature"), "{n}: {}", err(n));
    }
    assert!(err("xc1n0g08").contains("colour type 1"));
    assert!(err("xc9n2c08").contains("colour type 9"));
    assert!(err("xd0n2c08").contains("bit depth 0"));
    assert!(err("xd3n2c08").contains("bit depth 3"));
    assert!(err("xd9n2c08").contains("bit depth 99"));
    assert!(err("xdtn0g01").contains("no IDAT"));
    assert!(err("xhdn0g08").contains("IHDR chunk CRC"));
    assert!(err("xcsn0g01").contains("IDAT chunk CRC"));
}

#[test]
fn headers_match_the_file_names() {
    for (name, png) in decoded() {
        let i = &png.image;
        if name == "PngSuite" {
            assert_eq!((i.width, i.height), (256, 256));
            continue;
        }
        let b = name.as_bytes();
        // Names are ffi?cdd: feature, interlace (i/n), colour type, depth.
        let interlaced = b[3] == b'i';
        let ct = b[4] - b'0';
        let depth: u8 = name[6..8].parse().unwrap();
        assert_eq!(png.interlaced, interlaced, "{name}");
        assert_eq!(i.color_type.code(), ct, "{name}");
        assert_eq!(i.bit_depth, depth, "{name}");
        let size = match &name[..4] {
            "cdfn" => (8, 32),
            "cdhn" => (32, 8),
            "cdsn" => (8, 8),
            n if n.starts_with('s') => {
                let s: u32 = name[1..3].parse().unwrap();
                (s, s)
            }
            _ => (32, 32),
        };
        assert_eq!((i.width, i.height), size, "{name}");
    }
}

#[test]
fn same_picture_in_different_encodings_decodes_identically() {
    let d = decoded();
    let px = |n: &str| d[n].image.to_rgba16();
    let mut groups: Vec<Vec<String>> = Vec::new();
    // Interlaced and non-interlaced versions of the basic formats.
    for f in [
        "0g01", "0g02", "0g04", "0g08", "0g16", "2c08", "2c16", "3p01", "3p02", "3p04", "3p08",
        "4a08", "4a16", "6a08", "6a16",
    ] {
        groups.push(vec![format!("basi{f}"), format!("basn{f}")]);
    }
    for (s, f) in [
        (1, "3p01"),
        (2, "3p01"),
        (3, "3p01"),
        (4, "3p01"),
        (5, "3p02"),
        (6, "3p02"),
        (7, "3p02"),
        (8, "3p02"),
        (9, "3p02"),
        (32, "3p04"),
        (33, "3p04"),
        (34, "3p04"),
        (35, "3p04"),
        (36, "3p04"),
        (37, "3p04"),
        (38, "3p04"),
        (39, "3p04"),
        (40, "3p04"),
    ] {
        groups.push(vec![format!("s{s:02}i{f}"), format!("s{s:02}n{f}")]);
    }
    let g = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // Compression levels 0, 3, 6, 9.
    groups.push(g(&["z00n2c08", "z03n2c08", "z06n2c08", "z09n2c08"]));
    // Image data split over 1, 2, 4 IDAT chunks and one byte per chunk.
    groups.push(g(&[
        "basn0g16", "oi1n0g16", "oi2n0g16", "oi4n0g16", "oi9n0g16",
    ]));
    groups.push(g(&[
        "basn2c16", "oi1n2c16", "oi2n2c16", "oi4n2c16", "oi9n2c16",
    ]));
    // Background colour chunks do not change the pixels.
    groups.push(g(&["basn4a08", "bgai4a08", "bgbn4a08"]));
    groups.push(g(&["basn4a16", "bgai4a16", "bggn4a16"]));
    groups.push(g(&["basn6a08", "bgan6a08", "bgwn6a08"]));
    groups.push(g(&["basn6a16", "bgan6a16", "bgyn6a16"]));
    // Suggested palettes (sPLT, PLTE in truecolour) and histograms (hIST).
    groups.push(g(&["basn0g08", "ps1n0g08", "ps2n0g08"]));
    groups.push(g(&["basn2c16", "ps1n2c16", "ps2n2c16", "pp0n2c16"]));
    groups.push(g(&["basn3p04", "ch1n3p04"]));
    groups.push(g(&["basn3p08", "ch2n3p08"]));
    // tIME and text chunks.
    groups.push(g(&[
        "ct0n0g04", "ct1n0g04", "ctzn0g04", "cm0n0g04", "cm7n0g04", "cm9n0g04",
    ]));
    // The same colours as a palette and as truecolour: palette expansion
    // against direct samples.
    groups.push(g(&["cs5n2c08", "cs5n3p08"]));
    groups.push(g(&["cs8n2c08", "cs8n3p08"]));
    // Palette transparency with different backgrounds.
    groups.push(g(&[
        "tbbn3p08", "tbgn3p08", "tbwn3p08", "tbyn3p08", "tp1n3p08",
    ]));
    groups.push(g(&["tbbn2c16", "tbgn2c16"]));
    let mut compared = 0;
    for group in &groups {
        let first = px(&group[0]);
        for n in &group[1..] {
            assert!(px(n) == first, "{n} differs from {}", group[0]);
            compared += 1;
        }
    }
    eprintln!(
        "{} groups, {compared} images compared equal to their group's first",
        groups.len()
    );

    // The five filter files draw their filter number (in black) over the
    // same gradient: away from the digit the pixels agree.
    for ct in ["0g08", "2c08"] {
        let base = px(&format!("f00n{ct}"));
        for f in 1..=4 {
            let other = px(&format!("f0{f}n{ct}"));
            let mut differ = 0;
            for (a, b) in base.chunks(4).zip(other.chunks(4)) {
                if a != b {
                    differ += 1;
                    let black = |p: &[u16]| p[..3] == [0, 0, 0];
                    assert!(
                        black(a) || black(b),
                        "f0{f}n{ct}: a non-glyph pixel differs"
                    );
                }
            }
            assert!(differ < 120, "f0{f}n{ct}: {differ} pixels differ");
        }
    }
}

#[test]
fn metadata_matches_the_suite_descriptions() {
    let d = decoded();
    let m = |n: &str| &d[n].metadata;
    for (g, gamma) in [
        ("g03", 35000),
        ("g04", 45000),
        ("g05", 55000),
        ("g07", 70000),
        ("g10", 100000),
        ("g25", 250000),
    ] {
        for f in ["n0g16", "n2c08", "n3p04"] {
            assert_eq!(m(&format!("{g}{f}")).gamma, Some(gamma), "{g}{f}");
        }
    }
    assert_eq!(m("bgbn4a08").background, Some(Background::Gray(0)));
    assert_eq!(
        m("bgwn6a08").background,
        Some(Background::Rgb(255, 255, 255))
    );
    assert_eq!(
        m("bgyn6a16").background,
        Some(Background::Rgb(65535, 65535, 0))
    );
    assert_eq!(m("tbrn2c08").background, Some(Background::Rgb(255, 0, 0)));
    assert_eq!(m("tbgn2c16").background, Some(Background::Rgb(0, 65535, 0)));
    assert_eq!(m("tbwn0g16").background, Some(Background::Gray(65535)));
    let t = m("cm0n0g04").time.unwrap();
    assert_eq!(
        (t.year, t.month, t.day, t.hour, t.minute, t.second),
        (2000, 1, 1, 12, 34, 56)
    );
    let t = m("cm7n0g04").time.unwrap();
    assert_eq!(
        (t.year, t.month, t.day, t.hour, t.minute, t.second),
        (1970, 1, 1, 0, 0, 0)
    );
    let t = m("cm9n0g04").time.unwrap();
    assert_eq!(
        (t.year, t.month, t.day, t.hour, t.minute, t.second),
        (1999, 12, 31, 23, 59, 59)
    );
    let p = |n: &str| m(n).physical.map(|p| (p.x, p.y, p.metre));
    assert_eq!(p("cdfn2c08"), Some((1, 4, false)));
    assert_eq!(p("cdhn2c08"), Some((4, 1, false)));
    assert_eq!(p("cdsn2c08"), Some((1, 1, false)));
    assert_eq!(p("cdun2c08"), Some((1000, 1000, true)));
    assert_eq!(
        m("cs3n2c16").significant_bits.as_deref(),
        Some(&[13, 13, 13][..])
    );
    assert_eq!(
        m("cs5n2c08").significant_bits.as_deref(),
        Some(&[5, 5, 5][..])
    );
    assert_eq!(
        m("cs3n3p08").significant_bits.as_deref(),
        Some(&[3, 3, 3][..])
    );
    let c = m("ccwn2c08").chromaticities.unwrap();
    assert_eq!(
        (c.white, c.red, c.green, c.blue),
        (
            (31270, 32900),
            (64000, 33000),
            (30000, 60000),
            (15000, 6000)
        )
    );
    // Exif: a TIFF header, big-endian.
    assert_eq!(&m("exif2c08").exif.as_ref().unwrap()[..4], b"MM\0*");
    // Text: none; six tEXt; tEXt and zTXt; iTXt in five languages.
    assert!(m("ct0n0g04").text.is_empty());
    assert_eq!(m("ct1n0g04").text.len(), 6);
    assert!(m("ct1n0g04").text.iter().all(|t| t.kind == TextKind::Plain));
    assert_eq!(m("ct1n0g04").text[0].keyword, "Title");
    assert_eq!(m("ct1n0g04").text[0].text, "PngSuite");
    let z = &m("ctzn0g04").text;
    assert!(z.iter().any(|t| t.kind == TextKind::Compressed));
    assert_eq!(
        z.iter().map(|t| &t.text).collect::<Vec<_>>(),
        m("ct1n0g04")
            .text
            .iter()
            .map(|t| &t.text)
            .collect::<Vec<_>>()
    );
    for (f, lang, title) in [
        ("cten", "en", "Title"),
        ("ctfn", "fi", "Otsikko"),
        ("ctgn", "el", "Τίτλος"),
        ("cthn", "hi", "शीर्षक"),
        ("ctjn", "ja", "タイトル"),
    ] {
        let t = &m(&format!("{f}0g04")).text;
        assert_eq!(t.len(), 6, "{f}");
        assert!(
            t.iter()
                .all(|t| t.kind == TextKind::International { compressed: false }
                    && t.language == lang)
        );
        assert_eq!(t[0].translated_keyword, title, "{f}");
        assert_eq!(t[0].text, "PngSuite");
    }
    assert!(m("ch1n3p04").unknown.iter().any(|u| &u.kind == b"hIST"));
    assert!(m("ps1n0g08").unknown.iter().any(|u| &u.kind == b"sPLT"));
}

#[test]
fn transparency_and_significant_bits() {
    let d = decoded();
    let alphas = |n: &str| {
        let mut a: Vec<u16> = d[n].image.to_rgba16().chunks(4).map(|p| p[3]).collect();
        a.sort();
        a.dedup();
        a
    };
    // tp0*: not transparent. tb*, tp1: tRNS with fully transparent pixels.
    for n in ["tp0n0g08", "tp0n2c08", "tp0n3p08"] {
        assert_eq!(alphas(n), vec![65535], "{n}");
        assert!(d[n].image.transparency.is_none());
    }
    for n in [
        "tbbn0g04", "tbbn2c16", "tbbn3p08", "tbrn2c08", "tbwn0g16", "tp1n3p08",
    ] {
        assert_eq!(alphas(n), vec![0, 65535], "{n}");
    }
    assert_eq!(
        d["tbbn0g04"].image.transparency,
        Some(Transparency::Gray(15))
    );
    assert_eq!(
        d["tbrn2c08"].image.transparency,
        Some(Transparency::Rgb(255, 255, 255))
    );
    // tm3n3p02: palette alpha 0, 85, 170 and opaque.
    assert_eq!(
        d["tm3n3p02"].image.transparency,
        Some(Transparency::Palette(vec![0, 85, 170]))
    );
    assert_eq!(alphas("tm3n3p02"), vec![0, 85 * 257, 170 * 257, 65535]);
    // sBIT n: the samples are n-bit values scaled to the full depth (by bit
    // replication or by multiplication with rounding; the suite uses the
    // latter for cs3n2c16), so the low bits follow from the high ones.
    for (n, sig, depth) in [("cs3n2c16", 13u32, 16u32), ("cs5n2c08", 5, 8)] {
        let i = &d[n].image;
        for y in 0..i.height {
            for x in 0..i.width {
                for c in 0..3 {
                    let v = i.sample(x, y, c) as u32;
                    let s = v >> (depth - sig);
                    let mut r = 0u32;
                    let mut filled = 0;
                    while filled < depth {
                        r = (r << sig) | s;
                        filled += sig;
                    }
                    let replicated = r >> (filled - depth);
                    let multiplied = ((s as u64 * ((1u64 << depth) - 1) * 2 + ((1u64 << sig) - 1))
                        / (2 * ((1u64 << sig) - 1))) as u32;
                    assert!(
                        v == replicated || v == multiplied,
                        "{n} ({x},{y}) channel {c}: {v}"
                    );
                }
            }
        }
    }
    // Sixteen-bit greyscale against eight-bit: basn0g16's gradient has 16
    // significant bits (not just a scaled 8-bit image).
    assert!(d["basn0g16"].image.to_rgba16().iter().any(|v| v % 257 != 0));
}

/// A second decoder for z00n2c08 only: zlib with stored blocks, filters
/// Sub and Paeth (all that file uses), 8-bit RGB; nothing shared with the
/// crate.
#[test]
fn uncompressed_file_matches_a_minimal_independent_decode() {
    let bytes = std::fs::read(dir().join("z00n2c08.png")).unwrap();
    let mut pos = 8;
    let mut z = Vec::new();
    while pos < bytes.len() {
        let len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        if &bytes[pos + 4..pos + 8] == b"IDAT" {
            z.extend_from_slice(&bytes[pos + 8..pos + 8 + len]);
        }
        pos += 12 + len;
    }
    // zlib header, then stored blocks: header byte (BFINAL, BTYPE 00), LEN,
    // NLEN, bytes.
    let mut p = 2;
    let mut raw = Vec::new();
    loop {
        let head = z[p];
        assert_eq!(head & 6, 0, "not a stored block");
        let len = u16::from_le_bytes([z[p + 1], z[p + 2]]) as usize;
        raw.extend_from_slice(&z[p + 5..p + 5 + len]);
        p += 5 + len;
        if head & 1 == 1 {
            break;
        }
    }
    let (w, h, bpp) = (32usize, 32usize, 3usize);
    let stride = w * bpp;
    let mut img = vec![0u8; stride * h];
    for y in 0..h {
        let f = raw[y * (stride + 1)];
        for x in 0..stride {
            let v = raw[y * (stride + 1) + 1 + x] as i32;
            let a = if x >= bpp {
                img[y * stride + x - bpp] as i32
            } else {
                0
            };
            let b = if y > 0 {
                img[(y - 1) * stride + x] as i32
            } else {
                0
            };
            let c = if x >= bpp && y > 0 {
                img[(y - 1) * stride + x - bpp] as i32
            } else {
                0
            };
            let pred = match f {
                0 => 0,
                1 => a,
                4 => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                f => panic!("filter {f} not expected in z00n2c08"),
            };
            img[y * stride + x] = ((v + pred) & 0xFF) as u8;
        }
    }
    let png = rpng::decode(&bytes).unwrap();
    assert_eq!(png.image.data, img);
}

#[test]
fn reencoding_with_every_filter_and_interlace_round_trips() {
    let mut runs = 0;
    for (name, png) in decoded() {
        for interlaced in [false, true] {
            for filter in [
                FilterStrategy::Fixed(Filter::None),
                FilterStrategy::Fixed(Filter::Sub),
                FilterStrategy::Fixed(Filter::Up),
                FilterStrategy::Fixed(Filter::Average),
                FilterStrategy::Fixed(Filter::Paeth),
                FilterStrategy::Adaptive,
                FilterStrategy::AdaptiveAlways,
            ] {
                let mut enc = Encoder::with_level(if interlaced { 9 } else { 0 });
                enc.filter = filter;
                enc.interlaced = interlaced;
                enc.metadata = png.metadata.clone();
                let bytes = enc
                    .encode(&png.image)
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
                let back =
                    rpng::decode(&bytes).unwrap_or_else(|e| panic!("{name} {filter:?}: {e}"));
                assert!(
                    back.image == png.image,
                    "{name} {filter:?} interlaced={interlaced}: pixels differ"
                );
                assert_eq!(back.metadata, png.metadata, "{name}: metadata");
                runs += 1;
            }
        }
    }
    eprintln!("{runs} re-encodes of PngSuite images decoded identically");
}

#[test]
fn every_truncation_and_bit_flip_of_a_valid_file_fails_cleanly() {
    let all = suite();
    let mut cases = 0;
    for (name, bytes) in all.iter().filter(|(n, _)| {
        [
            "basi3p02", "basn6a16", "ctzn0g04", "s35i3p04", "tbwn3p08", "z09n2c08",
        ]
        .contains(&n.as_str())
    }) {
        for cut in 0..bytes.len() {
            assert!(rpng::decode(&bytes[..cut]).is_err(), "{name} cut at {cut}");
            cases += 1;
        }
        for bit in 0..bytes.len() * 8 {
            let mut b = bytes.clone();
            b[bit / 8] ^= 1 << (bit % 8);
            // Any single flipped bit is caught by the signature, a CRC, or
            // a length that no longer frames the chunks; never a panic.
            let _ = rpng::decode(&b);
            let _ = rpng::Decoder::new().check_crc(false).decode(&b);
            cases += 1;
        }
    }
    eprintln!("{cases} truncated or bit-flipped files handled");
}
