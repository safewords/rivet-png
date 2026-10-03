//! Reading a PNG or APNG: chunks, image data, metadata, frames.

use crate::deflate::{self, Crc32, Inflater};
use crate::error::{Error, Result, invalid};
use crate::filter::{Filter, unfilter};
use crate::interlace::{pass_size, scatter};
use crate::types::*;

/// The eight-byte PNG signature.
pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The fields of IHDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Header {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bits per sample.
    pub bit_depth: u8,
    /// Colour type.
    pub color_type: ColorType,
    /// Whether the image data is Adam7-interlaced.
    pub interlaced: bool,
}

/// A decoded file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Png {
    /// The static image (IDAT): what a decoder that does not animate shows.
    pub image: Image,
    /// Whether the file was Adam7-interlaced.
    pub interlaced: bool,
    /// The ancillary chunks.
    pub metadata: Metadata,
    /// The animation, if the file is an APNG whose animation chunks are well
    /// formed.
    pub animation: Option<Animation>,
    /// Why the animation chunks were set aside, if the file has them but
    /// they are malformed (the APNG specification has a decoder then show
    /// the static image).
    pub animation_error: Option<String>,
}

/// Decoder settings.
#[derive(Debug, Clone, Copy)]
pub struct Decoder {
    check_crc: bool,
    max_pixels: u64,
    animation: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Decodes a PNG or APNG with the default settings: CRCs and Adler-32
/// checked, at most 2^28 pixels per image, frames decoded.
pub fn decode(bytes: &[u8]) -> Result<Png> {
    Decoder::new().decode(bytes)
}

/// Reads only the signature and IHDR.
pub fn read_header(bytes: &[u8]) -> Result<Header> {
    if bytes.len() < 8 || bytes[..8] != SIGNATURE {
        return Err(invalid("not a PNG signature"));
    }
    let mut chunks = Chunks { bytes, pos: 8, check_crc: true };
    let c = chunks.next_chunk()?;
    if &c.kind != b"IHDR" {
        return Err(invalid("the first chunk is not IHDR"));
    }
    parse_ihdr(c.data)
}

struct Chunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
}

struct Chunks<'a> {
    bytes: &'a [u8],
    pos: usize,
    check_crc: bool,
}

impl<'a> Chunks<'a> {
    fn next_chunk(&mut self) -> Result<Chunk<'a>> {
        let b = self.bytes;
        let head = b.get(self.pos..self.pos + 8).ok_or_else(|| invalid("truncated: the file ends before IEND"))?;
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        if len > 0x7FFF_FFFF {
            return Err(invalid("a chunk length above 2^31 - 1"));
        }
        let kind = [head[4], head[5], head[6], head[7]];
        if !kind.iter().all(|c| c.is_ascii_alphabetic()) {
            return Err(invalid(format!("chunk type {:?} is not four letters", kind)));
        }
        let start = self.pos + 8;
        let end = start + len as usize;
        let data = b.get(start..end).ok_or_else(|| invalid(format!("truncated {} chunk", name(&kind))))?;
        let crc = b.get(end..end + 4).ok_or_else(|| invalid(format!("truncated {} chunk", name(&kind))))?;
        if self.check_crc {
            let stored = u32::from_be_bytes([crc[0], crc[1], crc[2], crc[3]]);
            let mut c = Crc32::new();
            c.update(&kind);
            c.update(data);
            if c.finish() != stored {
                return Err(invalid(format!("{} chunk CRC mismatch", name(&kind))));
            }
        }
        self.pos = end + 4;
        Ok(Chunk { kind, data })
    }
}

fn name(kind: &[u8; 4]) -> String {
    String::from_utf8_lossy(kind).into_owned()
}

fn be32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
}

fn be16(d: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([d[at], d[at + 1]])
}

fn parse_ihdr(d: &[u8]) -> Result<Header> {
    if d.len() != 13 {
        return Err(invalid("IHDR is not 13 bytes"));
    }
    let (width, height) = (be32(d, 0), be32(d, 4));
    if width == 0 || height == 0 || width > 0x7FFF_FFFF || height > 0x7FFF_FFFF {
        return Err(invalid(format!("dimensions {width}x{height} out of range")));
    }
    let bit_depth = d[8];
    let color_type =
        ColorType::from_code(d[9]).ok_or_else(|| invalid(format!("colour type {} does not exist", d[9])))?;
    if !color_type.allows(bit_depth) {
        return Err(invalid(format!("colour type {} does not allow bit depth {bit_depth}", d[9])));
    }
    if d[10] != 0 {
        return Err(Error::Unsupported(format!("compression method {}", d[10])));
    }
    if d[11] != 0 {
        return Err(Error::Unsupported(format!("filter method {}", d[11])));
    }
    let interlaced = match d[12] {
        0 => false,
        1 => true,
        m => return Err(invalid(format!("interlace method {m}"))),
    };
    Ok(Header { width, height, bit_depth, color_type, interlaced })
}

#[derive(PartialEq)]
enum IdatState {
    Before,
    In,
    After,
}

/// A frame being collected: its control and its compressed data.
struct PendingFrame {
    control: FrameControl,
    data: Vec<u8>,
    is_default: bool,
}

impl Decoder {
    /// CRCs checked, 2^28 pixels at most, frames decoded.
    pub fn new() -> Self {
        Decoder { check_crc: true, max_pixels: 1 << 28, animation: true }
    }

    /// Whether chunk CRCs and the zlib Adler-32 are checked (default true).
    pub fn check_crc(mut self, check: bool) -> Self {
        self.check_crc = check;
        self
    }

    /// The most pixels an image (or frame) may have (default 2^28).
    pub fn max_pixels(mut self, max: u64) -> Self {
        self.max_pixels = max;
        self
    }

    /// Whether APNG frames are decoded (default true); when not, the
    /// animation chunks are skipped.
    pub fn animation(mut self, decode: bool) -> Self {
        self.animation = decode;
        self
    }

    /// Decodes `bytes`.
    pub fn decode(&self, bytes: &[u8]) -> Result<Png> {
        if bytes.len() < 8 || bytes[..8] != SIGNATURE {
            return Err(invalid("not a PNG signature"));
        }
        let mut chunks = Chunks { bytes, pos: 8, check_crc: self.check_crc };
        let first = chunks.next_chunk()?;
        if &first.kind != b"IHDR" {
            return Err(invalid("the first chunk is not IHDR"));
        }
        let header = parse_ihdr(first.data)?;
        let pixels = header.width as u64 * header.height as u64;
        if pixels > self.max_pixels {
            return Err(Error::Limit(format!("{}x{} pixels", header.width, header.height)));
        }
        let ct = header.color_type;
        let mut palette: Option<Vec<[u8; 3]>> = None;
        let mut trns: Option<Transparency> = None;
        let mut meta = Metadata::default();
        let mut idat = Vec::new();
        let mut idat_state = IdatState::Before;
        // APNG state.
        let mut actl: Option<(u32, u32)> = None;
        let mut anim_err: Option<String> = None;
        let mut frames: Vec<PendingFrame> = Vec::new();
        let mut next_seq = 0u32;

        loop {
            let c = chunks.next_chunk()?;
            let d = c.data;
            if &c.kind != b"IDAT" && idat_state == IdatState::In {
                idat_state = IdatState::After;
            }
            match &c.kind {
                b"IHDR" => return Err(invalid("a second IHDR")),
                b"IEND" => {
                    break;
                }
                b"PLTE" => {
                    if idat_state != IdatState::Before {
                        return Err(invalid("PLTE after IDAT"));
                    }
                    if palette.is_some() {
                        return Err(invalid("a second PLTE"));
                    }
                    if matches!(ct, ColorType::Grayscale | ColorType::GrayscaleAlpha) {
                        return Err(invalid("PLTE in a greyscale image"));
                    }
                    if d.is_empty() || d.len() % 3 != 0 || d.len() > 768 {
                        return Err(invalid(format!("PLTE of {} bytes", d.len())));
                    }
                    palette = Some(d.chunks_exact(3).map(|p| [p[0], p[1], p[2]]).collect());
                }
                b"IDAT" => {
                    if idat_state == IdatState::After {
                        return Err(invalid("IDAT chunks that are not consecutive"));
                    }
                    if ct == ColorType::Indexed && palette.is_none() {
                        return Err(invalid("an indexed image without PLTE before IDAT"));
                    }
                    idat_state = IdatState::In;
                    idat.extend_from_slice(d);
                }
                b"acTL" if self.animation => {
                    if idat_state != IdatState::Before || actl.is_some() || d.len() != 8 {
                        anim_err.get_or_insert_with(|| "misplaced, repeated or malformed acTL".into());
                    } else if be32(d, 0) == 0 {
                        anim_err.get_or_insert_with(|| "acTL with zero frames".into());
                    } else {
                        actl = Some((be32(d, 0), be32(d, 4)));
                    }
                }
                b"fcTL" if self.animation => {
                    match parse_fctl(d, &header, &mut next_seq) {
                        Ok(control) => {
                            let is_default = idat_state == IdatState::Before;
                            if is_default
                                && (control.x_offset, control.y_offset, control.width, control.height)
                                    != (0, 0, header.width, header.height)
                            {
                                anim_err.get_or_insert_with(|| "the default image's fcTL is not the full canvas".into());
                            }
                            if let Some(prev) = frames.last()
                                && !prev.is_default
                                && prev.data.is_empty()
                            {
                                anim_err.get_or_insert_with(|| "a frame without fdAT data".into());
                            }
                            frames.push(PendingFrame { control, data: Vec::new(), is_default });
                        }
                        Err(e) => {
                            anim_err.get_or_insert(e);
                        }
                    }
                }
                b"fdAT" if self.animation => {
                    if d.len() < 4 {
                        anim_err.get_or_insert_with(|| "fdAT shorter than its sequence number".into());
                    } else if be32(d, 0) != next_seq {
                        anim_err.get_or_insert_with(|| "fdAT out of sequence".into());
                    } else {
                        next_seq += 1;
                        match frames.last_mut() {
                            Some(f) if !f.is_default => f.data.extend_from_slice(&d[4..]),
                            _ => {
                                anim_err.get_or_insert_with(|| "fdAT without a preceding fcTL".into());
                            }
                        }
                    }
                }
                b"tRNS" => {
                    if idat_state == IdatState::Before {
                        trns = parse_trns(d, ct, palette.as_ref().map_or(0, |p| p.len()));
                    }
                }
                b"gAMA" if d.len() == 4 => meta.gamma = Some(be32(d, 0)),
                b"cHRM" if d.len() == 32 => {
                    let p = |i: usize| (be32(d, 8 * i), be32(d, 8 * i + 4));
                    meta.chromaticities = Some(Chromaticities { white: p(0), red: p(1), green: p(2), blue: p(3) });
                }
                b"sRGB" if d.len() == 1 => meta.srgb = Some(d[0]),
                b"iCCP" => {
                    if let Some(icc) = parse_iccp(d) {
                        meta.icc_profile = Some(icc);
                    }
                }
                b"cICP" if d.len() == 4 => {
                    meta.cicp = Some(Cicp {
                        color_primaries: d[0],
                        transfer_function: d[1],
                        matrix_coefficients: d[2],
                        full_range: d[3] != 0,
                    })
                }
                b"mDCV" if d.len() == 24 => {
                    let p = |i: usize| (be16(d, 4 * i), be16(d, 4 * i + 2));
                    meta.mastering_display = Some(MasteringDisplay {
                        primaries: [p(0), p(1), p(2)],
                        white: p(3),
                        max_luminance: be32(d, 16),
                        min_luminance: be32(d, 20),
                    });
                }
                b"cLLI" if d.len() == 8 => {
                    meta.content_light_level = Some(ContentLightLevel { max_cll: be32(d, 0), max_fall: be32(d, 4) })
                }
                b"sBIT" => {
                    let n = if ct == ColorType::Indexed { 3 } else { ct.channels() };
                    if d.len() == n {
                        meta.significant_bits = Some(d.to_vec());
                    }
                }
                b"bKGD" => {
                    meta.background = match (ct, d.len()) {
                        (ColorType::Indexed, 1) => Some(Background::Palette(d[0])),
                        (ColorType::Grayscale | ColorType::GrayscaleAlpha, 2) => Some(Background::Gray(be16(d, 0))),
                        (ColorType::Rgb | ColorType::Rgba, 6) => {
                            Some(Background::Rgb(be16(d, 0), be16(d, 2), be16(d, 4)))
                        }
                        _ => meta.background,
                    }
                }
                b"pHYs" if d.len() == 9 => {
                    meta.physical = Some(PhysicalDimensions { x: be32(d, 0), y: be32(d, 4), metre: d[8] == 1 })
                }
                b"tIME" if d.len() == 7 => {
                    meta.time = Some(Time {
                        year: be16(d, 0),
                        month: d[2],
                        day: d[3],
                        hour: d[4],
                        minute: d[5],
                        second: d[6],
                    })
                }
                b"eXIf" => meta.exif = Some(d.to_vec()),
                b"tEXt" | b"zTXt" | b"iTXt" => {
                    if let Some(t) = parse_text(&c.kind, d) {
                        meta.text.push(t);
                    }
                }
                k if k[0].is_ascii_uppercase() && !matches!(k, b"acTL" | b"fcTL" | b"fdAT") => {
                    return Err(Error::Unsupported(format!("unknown critical chunk {}", name(k))));
                }
                k if matches!(k, b"acTL" | b"fcTL" | b"fdAT") => {}
                k => meta.unknown.push(UnknownChunk { kind: *k, data: d.to_vec() }),
            }
        }
        if idat_state == IdatState::Before {
            return Err(invalid("no IDAT chunk"));
        }
        if ct == ColorType::Indexed && palette.is_none() {
            return Err(invalid("an indexed image without PLTE"));
        }
        let image = self.decode_image(
            &idat,
            header.width,
            header.height,
            &header,
            palette.clone(),
            trns.clone(),
        )?;

        let mut animation = None;
        if let Some((num_frames, num_plays)) = actl {
            if anim_err.is_none() && frames.len() as u32 != num_frames {
                anim_err = Some(format!("acTL says {num_frames} frames, the file has {}", frames.len()));
            }
            if anim_err.is_none() {
                match self.decode_frames(&frames, &image, &header, &palette, &trns) {
                    Ok(out) => {
                        animation = Some(Animation {
                            num_plays,
                            default_image_is_first_frame: frames.first().is_some_and(|f| f.is_default),
                            frames: out,
                        })
                    }
                    Err(e) => anim_err = Some(e.to_string()),
                }
            }
        } else if !frames.is_empty() && anim_err.is_none() {
            anim_err = Some("fcTL without acTL".into());
        }
        Ok(Png {
            image,
            interlaced: header.interlaced,
            metadata: meta,
            animation,
            animation_error: anim_err,
        })
    }

    fn decode_frames(
        &self,
        frames: &[PendingFrame],
        default: &Image,
        header: &Header,
        palette: &Option<Vec<[u8; 3]>>,
        trns: &Option<Transparency>,
    ) -> Result<Vec<Frame>> {
        let mut out = Vec::with_capacity(frames.len());
        for f in frames {
            let image = if f.is_default {
                default.clone()
            } else {
                if f.data.is_empty() {
                    return Err(invalid("a frame without fdAT data"));
                }
                self.decode_image(&f.data, f.control.width, f.control.height, header, palette.clone(), trns.clone())?
            };
            out.push(Frame { control: f.control, image });
        }
        Ok(out)
    }

    /// Inflates, unfilters and deinterlaces one image's data.
    fn decode_image(
        &self,
        zdata: &[u8],
        width: u32,
        height: u32,
        header: &Header,
        palette: Option<Vec<[u8; 3]>>,
        transparency: Option<Transparency>,
    ) -> Result<Image> {
        if width as u64 * height as u64 > self.max_pixels {
            return Err(Error::Limit(format!("{width}x{height} pixels")));
        }
        let (ct, depth) = (header.color_type, header.bit_depth);
        let bits = ct.channels() * depth as usize;
        let bpp = bits.div_ceil(8);
        let row = row_bytes(width, ct, depth);
        let expected: usize = if header.interlaced {
            (0..7)
                .map(|p| {
                    let (pw, ph) = pass_size(p, width, height);
                    if pw == 0 || ph == 0 { 0 } else { ph as usize * (1 + (pw as usize * bits).div_ceil(8)) }
                })
                .sum()
        } else {
            height as usize * (1 + row)
        };
        let raw = Inflater::new()
            .limit(expected)
            .size_hint(expected)
            .check_adler(self.check_crc)
            .zlib(zdata)
            .map_err(|e| match e {
                deflate::Error::Limit(_) => invalid("more image data than the image holds"),
                deflate::Error::Truncated => invalid("the image data is truncated"),
                e => Error::Deflate(e),
            })?
            .data;
        if raw.len() < expected {
            return Err(invalid(format!("{} bytes of image data, the image needs {expected}", raw.len())));
        }
        let mut data = vec![0u8; row * height as usize];
        if header.interlaced {
            let mut off = 0;
            for p in 0..7 {
                let (pw, ph) = pass_size(p, width, height);
                if pw == 0 || ph == 0 {
                    continue;
                }
                let prow = (pw as usize * bits).div_ceil(8);
                let pass = unfilter_rows(&raw[off..off + ph as usize * (1 + prow)], prow, ph as usize, bpp)?;
                off += ph as usize * (1 + prow);
                scatter(p, &pass, prow, width, height, bits, &mut data, row);
            }
        } else {
            let un = unfilter_rows(&raw, row, height as usize, bpp)?;
            data.copy_from_slice(&un);
            // Zero the spare low bits of each row's last byte.
            let spare = row * 8 - width as usize * bits;
            if spare > 0 {
                let mask = !((1u8 << spare) - 1);
                for r in data.chunks_exact_mut(row) {
                    r[row - 1] &= mask;
                }
            }
        }
        Ok(Image { width, height, color_type: ct, bit_depth: depth, palette, transparency, data })
    }
}

fn unfilter_rows(raw: &[u8], row: usize, rows: usize, bpp: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; row * rows];
    let zero = vec![0u8; row];
    for y in 0..rows {
        let src = &raw[y * (row + 1)..(y + 1) * (row + 1)];
        let f = Filter::from_code(src[0]).ok_or_else(|| invalid(format!("filter type {}", src[0])))?;
        let (before, cur) = out.split_at_mut(y * row);
        let cur = &mut cur[..row];
        cur.copy_from_slice(&src[1..]);
        let prev = if y == 0 { &zero[..] } else { &before[(y - 1) * row..] };
        unfilter(f, cur, prev, bpp);
    }
    Ok(out)
}

fn parse_trns(d: &[u8], ct: ColorType, palette_len: usize) -> Option<Transparency> {
    match ct {
        ColorType::Grayscale if d.len() == 2 => Some(Transparency::Gray(be16(d, 0))),
        ColorType::Rgb if d.len() == 6 => Some(Transparency::Rgb(be16(d, 0), be16(d, 2), be16(d, 4))),
        ColorType::Indexed if palette_len > 0 && d.len() <= palette_len => Some(Transparency::Palette(d.to_vec())),
        _ => None,
    }
}

fn parse_fctl(d: &[u8], h: &Header, next_seq: &mut u32) -> std::result::Result<FrameControl, String> {
    if d.len() != 26 {
        return Err("fcTL is not 26 bytes".into());
    }
    if be32(d, 0) != *next_seq {
        return Err("fcTL out of sequence".into());
    }
    *next_seq += 1;
    let control = FrameControl {
        width: be32(d, 4),
        height: be32(d, 8),
        x_offset: be32(d, 12),
        y_offset: be32(d, 16),
        delay_num: be16(d, 20),
        delay_den: be16(d, 22),
        dispose: match d[24] {
            0 => DisposeOp::None,
            1 => DisposeOp::Background,
            2 => DisposeOp::Previous,
            _ => return Err("fcTL dispose_op above 2".into()),
        },
        blend: match d[25] {
            0 => BlendOp::Source,
            1 => BlendOp::Over,
            _ => return Err("fcTL blend_op above 1".into()),
        },
    };
    if control.width == 0
        || control.height == 0
        || control.x_offset as u64 + control.width as u64 > h.width as u64
        || control.y_offset as u64 + control.height as u64 > h.height as u64
    {
        return Err("an fcTL frame outside the canvas".into());
    }
    Ok(control)
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

fn keyword(b: &[u8]) -> Option<String> {
    if b.is_empty() || b.len() > 79 {
        return None;
    }
    Some(latin1(b))
}

fn inflate_text(d: &[u8]) -> Option<Vec<u8>> {
    // Text and profiles are small; 64 MiB bounds a hostile stream.
    Inflater::new().limit(64 << 20).zlib(d).ok().map(|r| r.data)
}

fn parse_text(kind: &[u8; 4], d: &[u8]) -> Option<Text> {
    let nul = d.iter().position(|&c| c == 0)?;
    let kw = keyword(&d[..nul])?;
    let rest = &d[nul + 1..];
    match kind {
        b"tEXt" => Some(Text {
            keyword: kw,
            text: latin1(rest),
            language: String::new(),
            translated_keyword: String::new(),
            kind: TextKind::Plain,
        }),
        b"zTXt" => {
            if rest.first() != Some(&0) {
                return None;
            }
            Some(Text {
                keyword: kw,
                text: latin1(&inflate_text(&rest[1..])?),
                language: String::new(),
                translated_keyword: String::new(),
                kind: TextKind::Compressed,
            })
        }
        _ => {
            if rest.len() < 2 {
                return None;
            }
            let compressed = match rest[0] {
                0 => false,
                1 => true,
                _ => return None,
            };
            if compressed && rest[1] != 0 {
                return None;
            }
            let rest = &rest[2..];
            let n1 = rest.iter().position(|&c| c == 0)?;
            let language = String::from_utf8(rest[..n1].to_vec()).ok()?;
            let rest = &rest[n1 + 1..];
            let n2 = rest.iter().position(|&c| c == 0)?;
            let translated = String::from_utf8(rest[..n2].to_vec()).ok()?;
            let body = &rest[n2 + 1..];
            let text = if compressed { inflate_text(body)? } else { body.to_vec() };
            Some(Text {
                keyword: kw,
                text: String::from_utf8(text).ok()?,
                language,
                translated_keyword: translated,
                kind: TextKind::International { compressed },
            })
        }
    }
}

fn parse_iccp(d: &[u8]) -> Option<IccProfile> {
    let nul = d.iter().position(|&c| c == 0)?;
    let name = keyword(&d[..nul])?;
    if d.get(nul + 1) != Some(&0) {
        return None;
    }
    let profile = inflate_text(&d[nul + 2..])?;
    Some(IccProfile { name, profile })
}

/// Draws an animation's frames onto its canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedFrame {
    /// The full canvas after this frame was drawn, 16-bit RGBA (as
    /// [`Image::to_rgba16`]).
    pub rgba16: Vec<u16>,
    /// Delay numerator.
    pub delay_num: u16,
    /// Delay denominator (0 read as 100).
    pub delay_den: u16,
}

impl ComposedFrame {
    /// The canvas as 8-bit RGBA, rounded.
    pub fn to_rgba8(&self) -> Vec<u8> {
        self.rgba16.iter().map(|&v| ((v as u32 * 255 + 32895) >> 16) as u8).collect()
    }
}

impl Animation {
    /// The canvas as shown after each frame, applying each frame's blend
    /// operation and then, before the next frame, its dispose operation
    /// (APNG: the canvas starts fully transparent black; a first frame's
    /// "previous" disposal is treated as "background").
    pub fn compose(&self, width: u32, height: u32) -> Vec<ComposedFrame> {
        let (cw, ch) = (width as usize, height as usize);
        let mut canvas = vec![0u16; cw * ch * 4];
        let mut out = Vec::with_capacity(self.frames.len());
        for (i, f) in self.frames.iter().enumerate() {
            let c = &f.control;
            let (fx, fy, fw, fh) = (c.x_offset as usize, c.y_offset as usize, c.width as usize, c.height as usize);
            if fx + fw > cw || fy + fh > ch || f.image.width as usize != fw || f.image.height as usize != fh {
                continue;
            }
            let mut dispose = c.dispose;
            if i == 0 && dispose == DisposeOp::Previous {
                dispose = DisposeOp::Background;
            }
            let saved: Option<Vec<u16>> = (dispose == DisposeOp::Previous).then(|| {
                let mut s = Vec::with_capacity(fw * fh * 4);
                for y in fy..fy + fh {
                    s.extend_from_slice(&canvas[(y * cw + fx) * 4..(y * cw + fx + fw) * 4]);
                }
                s
            });
            let src = f.image.to_rgba16();
            for y in 0..fh {
                for x in 0..fw {
                    let s = &src[(y * fw + x) * 4..(y * fw + x) * 4 + 4];
                    let di = ((fy + y) * cw + fx + x) * 4;
                    let d = &mut canvas[di..di + 4];
                    match c.blend {
                        BlendOp::Source => d.copy_from_slice(s),
                        BlendOp::Over => over(s, d),
                    }
                }
            }
            out.push(ComposedFrame { rgba16: canvas.clone(), delay_num: c.delay_num, delay_den: c.delay_den });
            match dispose {
                DisposeOp::None => {}
                DisposeOp::Background => {
                    for y in fy..fy + fh {
                        canvas[(y * cw + fx) * 4..(y * cw + fx + fw) * 4].fill(0);
                    }
                }
                DisposeOp::Previous => {
                    let s = saved.unwrap();
                    for (j, y) in (fy..fy + fh).enumerate() {
                        canvas[(y * cw + fx) * 4..(y * cw + fx + fw) * 4]
                            .copy_from_slice(&s[j * fw * 4..(j + 1) * fw * 4]);
                    }
                }
            }
        }
        out
    }
}

/// `s` over `d`, both straight (non-premultiplied) 16-bit RGBA.
fn over(s: &[u16], d: &mut [u16]) {
    let sa = s[3] as f64 / 65535.0;
    if s[3] == 65535 {
        d.copy_from_slice(s);
        return;
    }
    if s[3] == 0 {
        return;
    }
    let da = d[3] as f64 / 65535.0;
    let oa = sa + da * (1.0 - sa);
    for k in 0..3 {
        let v = (s[k] as f64 * sa + d[k] as f64 * da * (1.0 - sa)) / oa;
        d[k] = v.round().clamp(0.0, 65535.0) as u16;
    }
    d[3] = (oa * 65535.0).round() as u16;
}
