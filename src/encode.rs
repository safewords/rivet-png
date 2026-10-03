//! Writing a PNG or APNG.

use crate::decode::SIGNATURE;
use crate::deflate::{self, Crc32};
use crate::error::{Result, config};
use crate::filter::{FilterStrategy, filter_rows};
use crate::interlace::gather;
use crate::types::*;

/// Encoder settings and the metadata to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoder {
    /// DEFLATE settings for the image data (and compressed text and
    /// profiles): level 0-9, block type.
    pub compression: deflate::Options,
    /// How rows are filtered.
    pub filter: FilterStrategy,
    /// Whether to write Adam7-interlaced data.
    pub interlaced: bool,
    /// The most bytes per IDAT (or fdAT) chunk; 0 for the default, 1 MiB.
    pub chunk_size: usize,
    /// Ancillary chunks to write.
    pub metadata: Metadata,
}

impl Default for Encoder {
    fn default() -> Self {
        Encoder {
            compression: deflate::Options::default(),
            filter: FilterStrategy::Adaptive,
            interlaced: false,
            chunk_size: 0,
            metadata: Metadata::default(),
        }
    }
}

/// Encodes `image` with the default settings (level 6, adaptive filtering,
/// not interlaced).
pub fn encode(image: &Image) -> Result<Vec<u8>> {
    Encoder::default().encode(image)
}

struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn chunk(&mut self, kind: &[u8; 4], data: &[u8]) {
        self.out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        self.out.extend_from_slice(kind);
        self.out.extend_from_slice(data);
        let mut c = Crc32::new();
        c.update(kind);
        c.update(data);
        self.out.extend_from_slice(&c.finish().to_be_bytes());
    }
}

fn latin1(s: &str, what: &str) -> Result<Vec<u8>> {
    s.chars()
        .map(|c| u8::try_from(c as u32).map_err(|_| config(format!("{what} {s:?} is not Latin-1"))))
        .collect()
}

fn keyword(s: &str) -> Result<Vec<u8>> {
    let k = latin1(s, "keyword")?;
    if k.is_empty() || k.len() > 79 || k.contains(&0) {
        return Err(config(format!("keyword {s:?} must be 1-79 Latin-1 characters without NUL")));
    }
    Ok(k)
}

impl Encoder {
    /// The default settings with compression level `level`.
    pub fn with_level(level: u8) -> Encoder {
        Encoder { compression: deflate::Options { level, ..Default::default() }, ..Default::default() }
    }

    /// Encodes a still image.
    pub fn encode(&self, image: &Image) -> Result<Vec<u8>> {
        image.validate()?;
        let mut w = self.start(image)?;
        let z = self.image_data(image);
        self.idat(&mut w, &z);
        w.chunk(b"IEND", &[]);
        Ok(w.out)
    }

    /// Encodes an APNG. With `animation.default_image_is_first_frame`, the
    /// first frame (which must cover the canvas) is the static image and
    /// `static_image` must be `None`; otherwise `static_image` is the image
    /// shown by decoders that do not animate, sets the canvas size, and is
    /// not part of the animation. Every frame must have the static image's
    /// colour type, bit depth, palette and transparency, and fit the canvas.
    pub fn encode_animation(&self, animation: &Animation, static_image: Option<&Image>) -> Result<Vec<u8>> {
        if animation.frames.is_empty() {
            return Err(config("an animation with no frames"));
        }
        let base = match (animation.default_image_is_first_frame, static_image) {
            (true, None) => &animation.frames[0].image,
            (false, Some(img)) => img,
            (true, Some(_)) => return Err(config("a static image given, but the first frame is the static image")),
            (false, None) => return Err(config("no static image given, and the first frame is not one")),
        };
        base.validate()?;
        for (i, f) in animation.frames.iter().enumerate() {
            let (c, img) = (&f.control, &f.image);
            img.validate()?;
            if img.color_type != base.color_type
                || img.bit_depth != base.bit_depth
                || img.palette != base.palette
                || img.transparency != base.transparency
            {
                return Err(config(format!("frame {i}'s format differs from the static image's")));
            }
            if (img.width, img.height) != (c.width, c.height)
                || c.x_offset as u64 + c.width as u64 > base.width as u64
                || c.y_offset as u64 + c.height as u64 > base.height as u64
            {
                return Err(config(format!("frame {i} does not fit the canvas or its fcTL")));
            }
            if i == 0
                && animation.default_image_is_first_frame
                && (c.x_offset, c.y_offset, c.width, c.height) != (0, 0, base.width, base.height)
            {
                return Err(config("the first frame is the static image but does not cover the canvas"));
            }
        }
        let mut w = self.start(base)?;
        let mut actl = Vec::with_capacity(8);
        actl.extend_from_slice(&(animation.frames.len() as u32).to_be_bytes());
        actl.extend_from_slice(&animation.num_plays.to_be_bytes());
        w.chunk(b"acTL", &actl);
        let mut seq = 0u32;
        let mut rest = &animation.frames[..];
        if animation.default_image_is_first_frame {
            fctl(&mut w, &mut seq, &animation.frames[0].control);
            rest = &animation.frames[1..];
        }
        let z = self.image_data(base);
        self.idat(&mut w, &z);
        let size = if self.chunk_size == 0 { 1 << 20 } else { self.chunk_size };
        for f in rest {
            fctl(&mut w, &mut seq, &f.control);
            let z = self.image_data(&f.image);
            for piece in z.chunks(size) {
                let mut d = Vec::with_capacity(piece.len() + 4);
                d.extend_from_slice(&seq.to_be_bytes());
                d.extend_from_slice(piece);
                w.chunk(b"fdAT", &d);
                seq += 1;
            }
        }
        w.chunk(b"IEND", &[]);
        Ok(w.out)
    }

    fn idat(&self, w: &mut Writer, z: &[u8]) {
        let size = if self.chunk_size == 0 { 1 << 20 } else { self.chunk_size };
        for piece in z.chunks(size) {
            w.chunk(b"IDAT", piece);
        }
    }

    /// The zlib stream of an image's filtered (and interlaced) rows.
    fn image_data(&self, image: &Image) -> Vec<u8> {
        let bits = image.bits_per_pixel();
        let bpp = bits.div_ceil(8);
        let low = image.color_type == ColorType::Indexed || image.bit_depth < 8;
        let row = image.row_bytes();
        let mut filtered = Vec::with_capacity(image.data.len() + image.height as usize * 2);
        if self.interlaced {
            for p in 0..7 {
                let (pass, prow) = gather(p, &image.data, row, image.width, image.height, bits);
                filter_rows(&pass, prow, bpp, self.filter, low, &mut filtered);
            }
        } else {
            filter_rows(&image.data, row, bpp, self.filter, low, &mut filtered);
        }
        deflate::zlib_compress_with(&filtered, &self.compression)
    }

    /// Signature, IHDR and every chunk that precedes the image data.
    fn start(&self, image: &Image) -> Result<Writer> {
        let m = &self.metadata;
        let mut w = Writer { out: SIGNATURE.to_vec() };
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&image.width.to_be_bytes());
        ihdr.extend_from_slice(&image.height.to_be_bytes());
        ihdr.extend_from_slice(&[image.bit_depth, image.color_type.code(), 0, 0, self.interlaced as u8]);
        w.chunk(b"IHDR", &ihdr);
        let level = self.compression.level;

        // Before PLTE: colour space information, sBIT, HDR metadata.
        if let Some(c) = &m.cicp {
            w.chunk(b"cICP", &[c.color_primaries, c.transfer_function, c.matrix_coefficients, c.full_range as u8]);
        }
        if let Some(icc) = &m.icc_profile {
            let mut d = keyword(&icc.name)?;
            d.extend_from_slice(&[0, 0]);
            d.extend_from_slice(&deflate::zlib_compress(&icc.profile, level.max(1)));
            w.chunk(b"iCCP", &d);
        }
        if let Some(i) = m.srgb {
            if i > 3 {
                return Err(config("sRGB rendering intent above 3"));
            }
            w.chunk(b"sRGB", &[i]);
        }
        if let Some(g) = m.gamma {
            w.chunk(b"gAMA", &g.to_be_bytes());
        }
        if let Some(c) = &m.chromaticities {
            let mut d = Vec::with_capacity(32);
            for (x, y) in [c.white, c.red, c.green, c.blue] {
                d.extend_from_slice(&x.to_be_bytes());
                d.extend_from_slice(&y.to_be_bytes());
            }
            w.chunk(b"cHRM", &d);
        }
        if let Some(md) = &m.mastering_display {
            let mut d = Vec::with_capacity(24);
            for (x, y) in md.primaries.iter().chain(std::iter::once(&md.white)) {
                d.extend_from_slice(&x.to_be_bytes());
                d.extend_from_slice(&y.to_be_bytes());
            }
            d.extend_from_slice(&md.max_luminance.to_be_bytes());
            d.extend_from_slice(&md.min_luminance.to_be_bytes());
            w.chunk(b"mDCV", &d);
        }
        if let Some(c) = &m.content_light_level {
            let mut d = c.max_cll.to_be_bytes().to_vec();
            d.extend_from_slice(&c.max_fall.to_be_bytes());
            w.chunk(b"cLLI", &d);
        }
        if let Some(s) = &m.significant_bits {
            let n = if image.color_type == ColorType::Indexed { 3 } else { image.color_type.channels() };
            if s.len() != n {
                return Err(config(format!("sBIT needs {n} values")));
            }
            w.chunk(b"sBIT", s);
        }

        if let Some(p) = &image.palette {
            let d: Vec<u8> = p.iter().flatten().copied().collect();
            w.chunk(b"PLTE", &d);
        }

        // After PLTE, before IDAT.
        match &image.transparency {
            None => {}
            Some(Transparency::Gray(g)) => w.chunk(b"tRNS", &g.to_be_bytes()),
            Some(Transparency::Rgb(r, g, b)) => {
                let mut d = r.to_be_bytes().to_vec();
                d.extend_from_slice(&g.to_be_bytes());
                d.extend_from_slice(&b.to_be_bytes());
                w.chunk(b"tRNS", &d);
            }
            Some(Transparency::Palette(a)) => w.chunk(b"tRNS", a),
        }
        if let Some(b) = &m.background {
            let d = match (*b, image.color_type) {
                (Background::Palette(i), ColorType::Indexed) => vec![i],
                (Background::Gray(g), ColorType::Grayscale | ColorType::GrayscaleAlpha) => g.to_be_bytes().to_vec(),
                (Background::Rgb(r, g, b), ColorType::Rgb | ColorType::Rgba) => {
                    [r.to_be_bytes(), g.to_be_bytes(), b.to_be_bytes()].concat()
                }
                _ => return Err(config("bKGD does not match the colour type")),
            };
            w.chunk(b"bKGD", &d);
        }
        if let Some(p) = &m.physical {
            let mut d = p.x.to_be_bytes().to_vec();
            d.extend_from_slice(&p.y.to_be_bytes());
            d.push(p.metre as u8);
            w.chunk(b"pHYs", &d);
        }
        if let Some(e) = &m.exif {
            w.chunk(b"eXIf", e);
        }
        if let Some(t) = &m.time {
            let mut d = t.year.to_be_bytes().to_vec();
            d.extend_from_slice(&[t.month, t.day, t.hour, t.minute, t.second]);
            w.chunk(b"tIME", &d);
        }
        for t in &m.text {
            let mut d = keyword(&t.keyword)?;
            d.push(0);
            let kind = match t.kind {
                TextKind::Plain => {
                    d.extend_from_slice(&latin1(&t.text, "tEXt text")?);
                    b"tEXt"
                }
                TextKind::Compressed => {
                    d.push(0);
                    d.extend_from_slice(&deflate::zlib_compress(&latin1(&t.text, "zTXt text")?, level.max(1)));
                    b"zTXt"
                }
                TextKind::International { compressed } => {
                    d.extend_from_slice(&[compressed as u8, 0]);
                    d.extend_from_slice(t.language.as_bytes());
                    d.push(0);
                    d.extend_from_slice(t.translated_keyword.as_bytes());
                    d.push(0);
                    if compressed {
                        d.extend_from_slice(&deflate::zlib_compress(t.text.as_bytes(), level.max(1)));
                    } else {
                        d.extend_from_slice(t.text.as_bytes());
                    }
                    b"iTXt"
                }
            };
            w.chunk(kind, &d);
        }
        for u in &m.unknown {
            if !u.kind.iter().all(|c| c.is_ascii_alphabetic()) || u.kind[0].is_ascii_uppercase() {
                return Err(config("an unknown chunk must have an ancillary (lower-case first letter) type"));
            }
            w.chunk(&u.kind, &u.data);
        }
        Ok(w)
    }
}

fn fctl(w: &mut Writer, seq: &mut u32, c: &FrameControl) {
    let mut d = Vec::with_capacity(26);
    for v in [*seq, c.width, c.height, c.x_offset, c.y_offset] {
        d.extend_from_slice(&v.to_be_bytes());
    }
    d.extend_from_slice(&c.delay_num.to_be_bytes());
    d.extend_from_slice(&c.delay_den.to_be_bytes());
    d.push(match c.dispose {
        DisposeOp::None => 0,
        DisposeOp::Background => 1,
        DisposeOp::Previous => 2,
    });
    d.push(match c.blend {
        BlendOp::Source => 0,
        BlendOp::Over => 1,
    });
    w.chunk(b"fcTL", &d);
    *seq += 1;
}
