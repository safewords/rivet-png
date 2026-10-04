//! The encoder: every colour type and bit depth, each filter and adaptive
//! selection, Adam7, levels, metadata and APNG, all round-tripped exactly;
//! plus the decoder's chunk rules on hand-built files.

mod common;

use common::Rng;
use rpng::deflate::{self, Crc32};
use rpng::*;

const COMBOS: [(ColorType, u8); 15] = [
    (ColorType::Grayscale, 1),
    (ColorType::Grayscale, 2),
    (ColorType::Grayscale, 4),
    (ColorType::Grayscale, 8),
    (ColorType::Grayscale, 16),
    (ColorType::Rgb, 8),
    (ColorType::Rgb, 16),
    (ColorType::Indexed, 1),
    (ColorType::Indexed, 2),
    (ColorType::Indexed, 4),
    (ColorType::Indexed, 8),
    (ColorType::GrayscaleAlpha, 8),
    (ColorType::GrayscaleAlpha, 16),
    (ColorType::Rgba, 8),
    (ColorType::Rgba, 16),
];

/// A random image of the given format, smooth enough in part that filters
/// matter, with a palette and transparency where the format allows.
fn image(r: &mut Rng, w: u32, h: u32, ct: ColorType, depth: u8) -> Image {
    let row = (w as usize * ct.channels() * depth as usize).div_ceil(8);
    let mut data = vec![0u8; row * h as usize];
    let samples = w as usize * ct.channels();
    let max = if depth == 16 {
        65535u32
    } else {
        (1u32 << depth) - 1
    };
    for y in 0..h as usize {
        for i in 0..samples {
            // Gradient plus noise in the lower half.
            let v = if y < h as usize / 2 {
                ((i * 7 + y * 3) as u32 * max / (samples * 7 + h as usize * 3).max(1) as u32)
                    .min(max)
            } else {
                (r.next() % (max as u64 + 1)) as u32
            };
            match depth {
                16 => data[y * row + 2 * i..y * row + 2 * i + 2]
                    .copy_from_slice(&(v as u16).to_be_bytes()),
                8 => data[y * row + i] = v as u8,
                d => {
                    let bit = i * d as usize;
                    data[y * row + bit / 8] |= (v as u8) << (8 - d as usize - bit % 8);
                }
            }
        }
    }
    let mut img = Image {
        width: w,
        height: h,
        color_type: ct,
        bit_depth: depth,
        palette: None,
        transparency: None,
        data,
    };
    if ct == ColorType::Indexed {
        let n = 1usize << depth;
        img.palette = Some(
            (0..n)
                .map(|_| {
                    [
                        (r.next() >> 56) as u8,
                        (r.next() >> 56) as u8,
                        (r.next() >> 56) as u8,
                    ]
                })
                .collect(),
        );
        img.transparency = Some(Transparency::Palette(
            (0..n / 2).map(|i| (i * 37) as u8).collect(),
        ));
    }
    if ct == ColorType::Grayscale {
        img.transparency = Some(Transparency::Gray(max as u16 / 3));
    }
    if ct == ColorType::Rgb {
        img.transparency = Some(Transparency::Rgb(1, 2, 3));
        img.palette = Some(vec![[1, 2, 3], [200, 100, 50]]); // a suggested palette
    }
    img.validate().unwrap();
    img
}

const STRATEGIES: [FilterStrategy; 7] = [
    FilterStrategy::Fixed(Filter::None),
    FilterStrategy::Fixed(Filter::Sub),
    FilterStrategy::Fixed(Filter::Up),
    FilterStrategy::Fixed(Filter::Average),
    FilterStrategy::Fixed(Filter::Paeth),
    FilterStrategy::Adaptive,
    FilterStrategy::AdaptiveAlways,
];

#[test]
fn every_format_filter_interlace_and_level_round_trips() {
    let mut r = Rng(42);
    let mut runs = 0;
    for (ct, depth) in COMBOS {
        for (w, h) in [(1, 1), (7, 3), (33, 17), (64, 1), (1, 9)] {
            let img = image(&mut r, w, h, ct, depth);
            for interlaced in [false, true] {
                for filter in STRATEGIES {
                    for level in [0, 1, 6, 9] {
                        let enc = Encoder {
                            filter,
                            interlaced,
                            ..Encoder::with_level(level)
                        };
                        let png = enc.encode(&img).unwrap();
                        let back = decode(&png).unwrap();
                        assert!(
                            back.image == img,
                            "{ct:?}/{depth} {w}x{h} {filter:?} interlaced={interlaced} L{level}"
                        );
                        assert_eq!(back.interlaced, interlaced);
                        runs += 1;
                    }
                }
            }
        }
    }
    eprintln!(
        "{runs} exact round trips (15 formats x 5 sizes x 2 interlace x 7 filter strategies x 4 levels)"
    );
}

/// The filter type bytes of a non-interlaced file's rows.
fn filter_bytes(png: &[u8]) -> Vec<u8> {
    let hdr = read_header(png).unwrap();
    let mut pos = 8;
    let mut z = Vec::new();
    while pos < png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        if &png[pos + 4..pos + 8] == b"IDAT" {
            z.extend_from_slice(&png[pos + 8..pos + 8 + len]);
        }
        pos += 12 + len;
    }
    let raw = deflate::zlib_decompress(&z).unwrap();
    let row =
        (hdr.width as usize * hdr.color_type.channels() * hdr.bit_depth as usize).div_ceil(8) + 1;
    raw.chunks(row).map(|r| r[0]).collect()
}

#[test]
fn filters_are_the_ones_asked_for() {
    let mut r = Rng(7);
    let img = image(&mut r, 40, 20, ColorType::Rgb, 8);
    for f in Filter::ALL {
        let png = Encoder {
            filter: FilterStrategy::Fixed(f),
            ..Default::default()
        }
        .encode(&img)
        .unwrap();
        assert!(filter_bytes(&png).iter().all(|&b| b == f.code()), "{f:?}");
    }
    // Adaptive on a smooth image picks prediction, and beats no filtering.
    let smooth = {
        let mut d = Vec::new();
        for y in 0..64u32 {
            for x in 0..64u32 {
                d.extend_from_slice(&[(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8]);
            }
        }
        Image::new(64, 64, ColorType::Rgb, 8, d).unwrap()
    };
    let adaptive = Encoder::default().encode(&smooth).unwrap();
    let none = Encoder {
        filter: FilterStrategy::Fixed(Filter::None),
        ..Default::default()
    }
    .encode(&smooth)
    .unwrap();
    let used = filter_bytes(&adaptive);
    assert!(used.iter().any(|&b| b != 0), "{used:?}");
    assert!(
        adaptive.len() < none.len(),
        "adaptive {} vs none {}",
        adaptive.len(),
        none.len()
    );
    // Adaptive leaves indexed and sub-byte images unfiltered; AdaptiveAlways
    // does not.
    let idx = image(&mut r, 40, 20, ColorType::Indexed, 4);
    assert!(
        filter_bytes(&Encoder::default().encode(&idx).unwrap())
            .iter()
            .all(|&b| b == 0)
    );
    let always = Encoder {
        filter: FilterStrategy::AdaptiveAlways,
        ..Default::default()
    };
    let gray = image(&mut r, 40, 20, ColorType::Grayscale, 8);
    assert!(
        filter_bytes(&always.encode(&gray).unwrap())
            .iter()
            .any(|&b| b != 0)
    );
}

#[test]
fn interlaced_output_is_adam7() {
    let mut r = Rng(9);
    let img = image(&mut r, 13, 11, ColorType::Rgba, 16);
    let png = Encoder {
        interlaced: true,
        ..Default::default()
    }
    .encode(&img)
    .unwrap();
    assert!(read_header(&png).unwrap().interlaced);
    // The stream holds the seven passes: each pass's rows with a filter
    // byte, empty passes contributing nothing.
    let mut pos = 8;
    let mut z = Vec::new();
    while pos < png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        if &png[pos + 4..pos + 8] == b"IDAT" {
            z.extend_from_slice(&png[pos + 8..pos + 8 + len]);
        }
        pos += 12 + len;
    }
    let raw = deflate::zlib_decompress(&z).unwrap();
    let passes = [
        (0, 0, 8, 8),
        (4, 0, 8, 8),
        (0, 4, 4, 8),
        (2, 0, 4, 4),
        (0, 2, 2, 4),
        (1, 0, 2, 2),
        (0, 1, 1, 2),
    ];
    let mut want = 0;
    for (x0, y0, dx, dy) in passes {
        let pw = (13u32.saturating_sub(x0)).div_ceil(dx) as usize;
        let ph = (11u32.saturating_sub(y0)).div_ceil(dy) as usize;
        if pw > 0 && ph > 0 {
            want += ph * (1 + pw * 8);
        }
    }
    assert_eq!(raw.len(), want);
    // And a 1x1 interlaced image has only the first pass.
    let one = image(&mut r, 1, 1, ColorType::Grayscale, 1);
    let png = Encoder {
        interlaced: true,
        ..Default::default()
    }
    .encode(&one)
    .unwrap();
    assert_eq!(decode(&png).unwrap().image, one);
}

#[test]
fn levels_trade_size() {
    let mut r = Rng(11);
    let img = image(&mut r, 200, 200, ColorType::Rgb, 8);
    let sizes: Vec<usize> = (0..=9)
        .map(|l| Encoder::with_level(l).encode(&img).unwrap().len())
        .collect();
    eprintln!("PNG size by level: {sizes:?}");
    assert!(sizes[0] > sizes[1] && sizes[9] <= sizes[1]);
}

fn full_metadata() -> Metadata {
    Metadata {
        gamma: Some(45455),
        chromaticities: Some(Chromaticities {
            white: (31270, 32900),
            red: (64000, 33000),
            green: (30000, 60000),
            blue: (15000, 6000),
        }),
        srgb: Some(0),
        icc_profile: Some(IccProfile {
            name: "test profile".into(),
            profile: b"not really ICC ".repeat(40),
        }),
        cicp: Some(Cicp {
            color_primaries: 9,
            transfer_function: 16,
            matrix_coefficients: 0,
            full_range: true,
        }),
        mastering_display: Some(MasteringDisplay {
            primaries: [(35400, 14600), (8500, 39850), (6550, 2300)],
            white: (15635, 16450),
            max_luminance: 10_000_000,
            min_luminance: 1,
        }),
        content_light_level: Some(ContentLightLevel {
            max_cll: 10_000_000,
            max_fall: 4_000_000,
        }),
        significant_bits: Some(vec![8, 8, 8]),
        background: Some(Background::Rgb(1, 2, 3)),
        physical: Some(PhysicalDimensions {
            x: 3780,
            y: 3780,
            metre: true,
        }),
        exif: Some(b"MM\0*\0\0\0\x08\0\0".to_vec()),
        time: Some(Time {
            year: 2026,
            month: 10,
            day: 3,
            hour: 1,
            minute: 2,
            second: 3,
        }),
        text: vec![
            Text::plain("Title", "caf\u{e9}"),
            Text {
                kind: TextKind::Compressed,
                ..Text::plain("Comment", &"long text ".repeat(50))
            },
            Text {
                keyword: "Description".into(),
                text: "\u{65e5}\u{672c}\u{8a9e}".into(),
                language: "ja".into(),
                translated_keyword: "\u{8aac}\u{660e}".into(),
                kind: TextKind::International { compressed: true },
            },
            Text {
                keyword: "Author".into(),
                text: "\u{395}\u{3bb}\u{3cd}".into(),
                language: "el".into(),
                translated_keyword: String::new(),
                kind: TextKind::International { compressed: false },
            },
        ],
        unknown: vec![UnknownChunk {
            kind: *b"prVt",
            data: vec![1, 2, 3],
        }],
    }
}

#[test]
fn metadata_round_trips() {
    let mut r = Rng(13);
    let img = image(&mut r, 9, 9, ColorType::Rgb, 8);
    let enc = Encoder {
        metadata: full_metadata(),
        ..Default::default()
    };
    let png = decode(&enc.encode(&img).unwrap()).unwrap();
    assert_eq!(png.metadata, full_metadata());
    assert_eq!(png.image, img);
}

#[test]
fn encoder_refuses_what_png_cannot_hold() {
    assert!(Image::new(2, 2, ColorType::Rgb, 4, vec![0; 3]).is_err());
    assert!(Image::new(2, 2, ColorType::Rgb, 8, vec![0; 11]).is_err());
    assert!(Image::new(0, 2, ColorType::Rgb, 8, vec![]).is_err());
    let idx = Image {
        palette: None,
        ..Image::new(1, 1, ColorType::Grayscale, 8, vec![0]).unwrap()
    };
    let idx = Image {
        color_type: ColorType::Indexed,
        ..idx
    };
    assert!(matches!(encode(&idx), Err(Error::Config(_))));
    let gray = Image::new(1, 1, ColorType::Grayscale, 8, vec![0]).unwrap();
    let mut enc = Encoder::default();
    enc.metadata.text.push(Text::plain("Title", "\u{3a9}")); // not Latin-1
    assert!(matches!(enc.encode(&gray), Err(Error::Config(_))));
    let mut enc = Encoder::default();
    enc.metadata.text.push(Text::plain("", "x"));
    assert!(enc.encode(&gray).is_err());
    let mut enc = Encoder::default();
    enc.metadata.unknown.push(UnknownChunk {
        kind: *b"PRVT",
        data: vec![],
    }); // critical
    assert!(enc.encode(&gray).is_err());
}

fn rgba(w: u32, h: u32, px: [u8; 4]) -> Image {
    Image::from_rgba8(w, h, px.repeat((w * h) as usize)).unwrap()
}

fn fc(w: u32, h: u32, x: u32, y: u32, dispose: DisposeOp, blend: BlendOp) -> FrameControl {
    FrameControl {
        width: w,
        height: h,
        x_offset: x,
        y_offset: y,
        delay_num: 1,
        delay_den: 10,
        dispose,
        blend,
    }
}

#[test]
fn apng_round_trips_and_composes() {
    // 4x4 canvas.
    // 0: full red, dispose none.
    // 1: 2x2 blue at half alpha over (1,1), blend over, dispose previous.
    // 2: 1x1 green at (0,0), source, dispose background.
    // 3: 4x4 fully transparent, blend over: the canvas shows through.
    let frames = vec![
        Frame {
            control: fc(4, 4, 0, 0, DisposeOp::None, BlendOp::Source),
            image: rgba(4, 4, [255, 0, 0, 255]),
        },
        Frame {
            control: fc(2, 2, 1, 1, DisposeOp::Previous, BlendOp::Over),
            image: rgba(2, 2, [0, 0, 255, 128]),
        },
        Frame {
            control: fc(1, 1, 0, 0, DisposeOp::Background, BlendOp::Source),
            image: rgba(1, 1, [0, 255, 0, 255]),
        },
        Frame {
            control: fc(4, 4, 0, 0, DisposeOp::None, BlendOp::Over),
            image: rgba(4, 4, [9, 9, 9, 0]),
        },
    ];
    let anim = Animation {
        num_plays: 3,
        default_image_is_first_frame: true,
        frames,
    };
    for interlaced in [false, true] {
        let enc = Encoder {
            interlaced,
            chunk_size: 7,
            ..Default::default()
        };
        let bytes = enc.encode_animation(&anim, None).unwrap();
        let png = decode(&bytes).unwrap();
        assert_eq!(png.animation_error, None);
        assert_eq!(png.animation.as_ref(), Some(&anim));
        assert_eq!(png.image, anim.frames[0].image);
        let out = png.animation.unwrap().compose(4, 4);
        assert_eq!(out.len(), 4);
        let at = |f: usize, x: usize, y: usize| -> [u8; 4] {
            out[f].to_rgba8()[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4]
                .try_into()
                .unwrap()
        };
        assert_eq!(at(0, 0, 0), [255, 0, 0, 255]);
        // Half-alpha blue over opaque red: (127, 0, 128) opaque.
        let p = at(1, 1, 1);
        assert_eq!(p[3], 255);
        assert!(
            (p[0] as i32 - 127).abs() <= 1 && p[1] == 0 && (p[2] as i32 - 128).abs() <= 1,
            "{p:?}"
        );
        assert_eq!(at(1, 0, 0), [255, 0, 0, 255]);
        // Frame 1 disposed to previous: red again under frame 2.
        assert_eq!(at(2, 1, 1), [255, 0, 0, 255]);
        assert_eq!(at(2, 0, 0), [0, 255, 0, 255]);
        // Frame 2 disposed to background: (0,0) transparent; the
        // transparent frame 3 blended over changes nothing.
        assert_eq!(at(3, 0, 0), [0, 0, 0, 0]);
        assert_eq!(at(3, 3, 3), [255, 0, 0, 255]);
        assert_eq!(out[3].delay_num, 1);
    }

    // A static image that is not part of the animation.
    let hidden = rgba(4, 4, [1, 2, 3, 4]);
    let anim2 = Animation {
        default_image_is_first_frame: false,
        ..anim.clone()
    };
    let png = decode(
        &Encoder::default()
            .encode_animation(&anim2, Some(&hidden))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(png.image, hidden);
    assert_eq!(png.animation, Some(anim2));

    // Indexed and 16-bit frames.
    let mut r = Rng(17);
    let base = image(&mut r, 8, 6, ColorType::Indexed, 2);
    let mut part = image(&mut r, 3, 2, ColorType::Indexed, 2);
    part.palette = base.palette.clone();
    part.transparency = base.transparency.clone();
    let anim3 = Animation {
        num_plays: 0,
        default_image_is_first_frame: true,
        frames: vec![
            Frame {
                control: FrameControl::full(8, 6),
                image: base.clone(),
            },
            Frame {
                control: fc(3, 2, 5, 4, DisposeOp::Background, BlendOp::Over),
                image: part,
            },
        ],
    };
    let png = decode(&Encoder::default().encode_animation(&anim3, None).unwrap()).unwrap();
    assert_eq!(png.animation, Some(anim3));

    // Frames that do not fit, or differ in format, are refused.
    let mut bad = anim.clone();
    bad.frames[1].control.x_offset = 3;
    assert!(Encoder::default().encode_animation(&bad, None).is_err());
    let mut bad = anim.clone();
    bad.frames[2].image = Image::new(1, 1, ColorType::Rgb, 8, vec![0; 3]).unwrap();
    assert!(Encoder::default().encode_animation(&bad, None).is_err());
}

/// Builds a file from chunks.
fn file(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    for (k, d) in chunks {
        out.extend_from_slice(&(d.len() as u32).to_be_bytes());
        out.extend_from_slice(*k);
        out.extend_from_slice(d);
        let mut c = Crc32::new();
        c.update(*k);
        c.update(d);
        out.extend_from_slice(&c.finish().to_be_bytes());
    }
    out
}

fn ihdr(w: u32, h: u32, depth: u8, ct: u8, interlace: u8) -> Vec<u8> {
    let mut d = w.to_be_bytes().to_vec();
    d.extend_from_slice(&h.to_be_bytes());
    d.extend_from_slice(&[depth, ct, 0, 0, interlace]);
    d
}

#[test]
fn decoder_enforces_chunk_rules() {
    // A 2x2 8-bit grey image: rows "0 a b".
    let z = deflate::zlib_compress(&[0, 10, 20, 0, 30, 40], 6);
    let (z1, z2) = (z[..4].to_vec(), z[4..].to_vec());
    let ok = file(&[
        (b"IHDR", ihdr(2, 2, 8, 0, 0)),
        (b"IDAT", z1.clone()),
        (b"IDAT", z2.clone()),
        (b"IEND", vec![]),
    ]);
    assert_eq!(decode(&ok).unwrap().image.data, vec![10, 20, 30, 40]);

    let err = |chunks: &[(&[u8; 4], Vec<u8>)]| decode(&file(chunks)).unwrap_err();
    // IDAT chunks separated by another chunk.
    let e = err(&[
        (b"IHDR", ihdr(2, 2, 8, 0, 0)),
        (b"IDAT", z1.clone()),
        (b"tEXt", b"a\0b".to_vec()),
        (b"IDAT", z2.clone()),
        (b"IEND", vec![]),
    ]);
    assert!(e.to_string().contains("consecutive"), "{e}");
    // An unknown critical chunk; an unknown ancillary one is kept.
    let e = err(&[
        (b"IHDR", ihdr(2, 2, 8, 0, 0)),
        (b"CRIT", vec![]),
        (b"IDAT", z.clone()),
        (b"IEND", vec![]),
    ]);
    assert!(matches!(e, Error::Unsupported(_)), "{e}");
    let png = decode(&file(&[
        (b"IHDR", ihdr(2, 2, 8, 0, 0)),
        (b"anCi", vec![5]),
        (b"IDAT", z.clone()),
        (b"IEND", vec![]),
    ]))
    .unwrap();
    assert_eq!(
        png.metadata.unknown,
        vec![UnknownChunk {
            kind: *b"anCi",
            data: vec![5]
        }]
    );
    // No IEND.
    assert!(
        err(&[(b"IHDR", ihdr(2, 2, 8, 0, 0)), (b"IDAT", z.clone())])
            .to_string()
            .contains("IEND")
    );
    // IHDR not first; a second IHDR.
    assert!(
        err(&[
            (b"gAMA", vec![0, 0, 0, 1]),
            (b"IHDR", ihdr(2, 2, 8, 0, 0)),
            (b"IDAT", z.clone()),
            (b"IEND", vec![])
        ])
        .to_string()
        .contains("first chunk")
    );
    assert!(
        err(&[
            (b"IHDR", ihdr(2, 2, 8, 0, 0)),
            (b"IHDR", ihdr(2, 2, 8, 0, 0)),
            (b"IDAT", z.clone()),
            (b"IEND", vec![])
        ])
        .to_string()
        .contains("second IHDR")
    );
    // PLTE in a greyscale image; indexed without PLTE; PLTE after IDAT.
    assert!(
        err(&[
            (b"IHDR", ihdr(2, 2, 8, 0, 0)),
            (b"PLTE", vec![0; 3]),
            (b"IDAT", z.clone()),
            (b"IEND", vec![])
        ])
        .to_string()
        .contains("greyscale")
    );
    assert!(
        err(&[
            (b"IHDR", ihdr(2, 2, 8, 3, 0)),
            (b"IDAT", z.clone()),
            (b"IEND", vec![])
        ])
        .to_string()
        .contains("PLTE")
    );
    assert!(
        err(&[
            (b"IHDR", ihdr(2, 2, 8, 3, 0)),
            (b"PLTE", vec![0; 3]),
            (b"IDAT", z.clone()),
            (b"PLTE", vec![0; 3]),
            (b"IEND", vec![])
        ])
        .to_string()
        .contains("PLTE")
    );
    // Filter type 5.
    let z5 = deflate::zlib_compress(&[5, 10, 20, 0, 30, 40], 6);
    assert!(
        err(&[
            (b"IHDR", ihdr(2, 2, 8, 0, 0)),
            (b"IDAT", z5),
            (b"IEND", vec![])
        ])
        .to_string()
        .contains("filter type")
    );
    // Too little and too much image data.
    let short = deflate::zlib_compress(&[0, 10, 20, 0, 30], 6);
    assert!(
        err(&[
            (b"IHDR", ihdr(2, 2, 8, 0, 0)),
            (b"IDAT", short),
            (b"IEND", vec![])
        ])
        .to_string()
        .contains("needs")
    );
    let long = deflate::zlib_compress(&[0, 10, 20, 0, 30, 40, 0], 6);
    assert!(
        err(&[
            (b"IHDR", ihdr(2, 2, 8, 0, 0)),
            (b"IDAT", long),
            (b"IEND", vec![])
        ])
        .to_string()
        .contains("more image data")
    );
    // Interlace method 2, filter method 1, compression method 1.
    assert!(
        err(&[
            (b"IHDR", ihdr(2, 2, 8, 0, 2)),
            (b"IDAT", z.clone()),
            (b"IEND", vec![])
        ])
        .to_string()
        .contains("interlace")
    );
    let mut h = ihdr(2, 2, 8, 0, 0);
    h[11] = 1;
    assert!(matches!(
        err(&[(b"IHDR", h), (b"IDAT", z.clone()), (b"IEND", vec![])]),
        Error::Unsupported(_)
    ));
    // A bad CRC is an error unless CRC checking is off.
    let mut bad = ok.clone();
    let n = bad.len();
    bad[n - 13] ^= 0xFF; // the last IDAT's CRC
    assert!(decode(&bad).is_err());
    assert!(Decoder::new().check_crc(false).decode(&bad).is_ok());
    // The pixel limit.
    assert!(matches!(
        Decoder::new().max_pixels(3).decode(&ok),
        Err(Error::Limit(_))
    ));
    // Bytes after IEND are ignored.
    let mut after = ok.clone();
    after.extend_from_slice(b"junk");
    assert!(decode(&after).is_ok());
}

#[test]
fn malformed_animation_falls_back_to_the_static_image() {
    let anim = Animation {
        num_plays: 0,
        default_image_is_first_frame: true,
        frames: vec![
            Frame {
                control: FrameControl::full(2, 2),
                image: rgba(2, 2, [1, 1, 1, 255]),
            },
            Frame {
                control: FrameControl::full(2, 2),
                image: rgba(2, 2, [2, 2, 2, 255]),
            },
        ],
    };
    let good = Encoder::default().encode_animation(&anim, None).unwrap();
    assert!(decode(&good).unwrap().animation.is_some());
    // Rewrite the acTL frame count to 3 (and fix its CRC).
    let mut bad = good.clone();
    let at = bad.windows(4).position(|w| w == b"acTL").unwrap();
    bad[at + 4..at + 8].copy_from_slice(&3u32.to_be_bytes());
    let mut c = Crc32::new();
    c.update(&bad[at..at + 12]);
    let crc = c.finish().to_be_bytes();
    bad[at + 12..at + 16].copy_from_slice(&crc);
    let png = decode(&bad).unwrap();
    assert!(png.animation.is_none());
    assert!(png.animation_error.unwrap().contains("3 frames"));
    assert_eq!(png.image, anim.frames[0].image);
    // Skipping animation entirely.
    let png = Decoder::new().animation(false).decode(&good).unwrap();
    assert!(png.animation.is_none() && png.animation_error.is_none());
}

#[test]
fn rgba_conversions() {
    // 16-bit to 8-bit rounds; 1-, 2- and 4-bit grey scale by replication.
    let img = Image::new(2, 1, ColorType::Grayscale, 16, vec![0x80, 0x7F, 0xFF, 0xFF]).unwrap();
    assert_eq!(img.to_rgba8(), vec![128, 128, 128, 255, 255, 255, 255, 255]);
    let img = Image::new(4, 1, ColorType::Grayscale, 2, vec![0b00_01_10_11]).unwrap();
    assert_eq!(
        img.to_rgba8().chunks(4).map(|p| p[0]).collect::<Vec<_>>(),
        vec![0, 85, 170, 255]
    );
    let img = Image::new(2, 1, ColorType::Grayscale, 4, vec![0x0F]).unwrap();
    assert_eq!(
        img.to_rgba16().chunks(4).map(|p| p[0]).collect::<Vec<_>>(),
        vec![0, 65535]
    );
}
