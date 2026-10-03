//! The image, its metadata and its animation, shared by the decoder and the
//! encoder.

use crate::error::{Result, config};

/// The colour type of IHDR (PNG §11.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorType {
    /// 0: one grey sample per pixel (bit depth 1, 2, 4, 8 or 16).
    Grayscale,
    /// 2: red, green, blue (8 or 16).
    Rgb,
    /// 3: a palette index (1, 2, 4 or 8).
    Indexed,
    /// 4: grey and alpha (8 or 16).
    GrayscaleAlpha,
    /// 6: red, green, blue and alpha (8 or 16).
    Rgba,
}

impl ColorType {
    /// The IHDR code.
    pub fn code(self) -> u8 {
        match self {
            ColorType::Grayscale => 0,
            ColorType::Rgb => 2,
            ColorType::Indexed => 3,
            ColorType::GrayscaleAlpha => 4,
            ColorType::Rgba => 6,
        }
    }

    /// The colour type with IHDR code `code`.
    pub fn from_code(code: u8) -> Option<ColorType> {
        Some(match code {
            0 => ColorType::Grayscale,
            2 => ColorType::Rgb,
            3 => ColorType::Indexed,
            4 => ColorType::GrayscaleAlpha,
            6 => ColorType::Rgba,
            _ => return None,
        })
    }

    /// Samples per pixel.
    pub fn channels(self) -> usize {
        match self {
            ColorType::Grayscale | ColorType::Indexed => 1,
            ColorType::GrayscaleAlpha => 2,
            ColorType::Rgb => 3,
            ColorType::Rgba => 4,
        }
    }

    /// Whether PNG allows this colour type at `bit_depth` (PNG §11.2.2,
    /// table 11.1).
    pub fn allows(self, bit_depth: u8) -> bool {
        match self {
            ColorType::Grayscale => matches!(bit_depth, 1 | 2 | 4 | 8 | 16),
            ColorType::Indexed => matches!(bit_depth, 1 | 2 | 4 | 8),
            _ => matches!(bit_depth, 8 | 16),
        }
    }
}

/// The transparency of a `tRNS` chunk (PNG §11.3.1.1). Sample values are
/// at the image's bit depth.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Transparency {
    /// Greyscale images: this grey value is fully transparent.
    Gray(u16),
    /// Truecolour images: this colour is fully transparent.
    Rgb(u16, u16, u16),
    /// Indexed images: the alpha of palette entries 0.., the rest opaque.
    Palette(Vec<u8>),
}

/// A decoded (or to-be-encoded) image: its samples in PNG's own layout,
/// with the palette and transparency needed to interpret them.
///
/// `data` holds `height` rows of [`row_bytes`](Image::row_bytes) each, no
/// filter bytes, no interlacing: samples packed most significant bits first
/// within a byte below eight bits, one byte each at eight, two bytes big
/// endian at sixteen. A sub-byte row's spare low bits are zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Colour type.
    pub color_type: ColorType,
    /// Bits per sample.
    pub bit_depth: u8,
    /// RGB palette entries (`PLTE`): required for [`ColorType::Indexed`],
    /// a suggestion for quantisation with [`ColorType::Rgb`] and
    /// [`ColorType::Rgba`], absent for the grey types.
    pub palette: Option<Vec<[u8; 3]>>,
    /// The `tRNS` chunk, for colour types 0, 2 and 3.
    pub transparency: Option<Transparency>,
    /// The samples.
    pub data: Vec<u8>,
}

impl Image {
    /// An image with no palette and no transparency; checks the size of
    /// `data` and the colour type / depth combination.
    pub fn new(width: u32, height: u32, color_type: ColorType, bit_depth: u8, data: Vec<u8>) -> Result<Image> {
        let img = Image { width, height, color_type, bit_depth, palette: None, transparency: None, data };
        img.validate()?;
        Ok(img)
    }

    /// An 8-bit RGBA image from `width * height * 4` bytes.
    pub fn from_rgba8(width: u32, height: u32, rgba: Vec<u8>) -> Result<Image> {
        Image::new(width, height, ColorType::Rgba, 8, rgba)
    }

    /// Bits per pixel.
    pub fn bits_per_pixel(&self) -> usize {
        self.color_type.channels() * self.bit_depth as usize
    }

    /// Bytes per row of `data`.
    pub fn row_bytes(&self) -> usize {
        row_bytes(self.width, self.color_type, self.bit_depth)
    }

    /// Checks the colour type and depth, the dimensions, `data`'s length and
    /// that an indexed image has a palette.
    pub fn validate(&self) -> Result<()> {
        if !self.color_type.allows(self.bit_depth) {
            return Err(config(format!(
                "colour type {} does not allow bit depth {}",
                self.color_type.code(),
                self.bit_depth
            )));
        }
        if self.width == 0 || self.height == 0 || self.width > 0x7FFF_FFFF || self.height > 0x7FFF_FFFF {
            return Err(config(format!("dimensions {}x{} out of range", self.width, self.height)));
        }
        let want = (self.row_bytes() as u128) * self.height as u128;
        if self.data.len() as u128 != want {
            return Err(config(format!("{} bytes of samples, the image needs {want}", self.data.len())));
        }
        if let Some(p) = &self.palette {
            if p.is_empty() || p.len() > 256 {
                return Err(config(format!("a palette of {} entries", p.len())));
            }
            if matches!(self.color_type, ColorType::Grayscale | ColorType::GrayscaleAlpha) {
                return Err(config("a palette on a greyscale image"));
            }
            if self.color_type == ColorType::Indexed && p.len() > 1 << self.bit_depth {
                return Err(config("more palette entries than the bit depth can index"));
            }
        } else if self.color_type == ColorType::Indexed {
            return Err(config("an indexed image without a palette"));
        }
        match (&self.transparency, self.color_type) {
            (None, _) => {}
            (Some(Transparency::Gray(_)), ColorType::Grayscale)
            | (Some(Transparency::Rgb(..)), ColorType::Rgb) => {}
            (Some(Transparency::Palette(a)), ColorType::Indexed) => {
                if a.len() > self.palette.as_ref().map_or(0, |p| p.len()) {
                    return Err(config("more tRNS entries than palette entries"));
                }
            }
            _ => return Err(config("tRNS does not match the colour type")),
        }
        Ok(())
    }

    /// The sample of channel `c` of pixel (`x`, `y`) at the image's depth.
    pub fn sample(&self, x: u32, y: u32, c: usize) -> u16 {
        let row = &self.data[y as usize * self.row_bytes()..];
        let ch = self.color_type.channels();
        sample_at(row, self.bit_depth, x as usize * ch + c)
    }

    /// The image as 16-bit RGBA, four values per pixel: palette applied,
    /// `tRNS` turned into alpha, samples below 16 bits scaled to the full
    /// range by bit replication (so 8-bit `v` becomes `v * 257`). A palette
    /// index past the end of the palette reads as opaque black.
    pub fn to_rgba16(&self) -> Vec<u16> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut out = Vec::with_capacity(w * h * 4);
        let depth = self.bit_depth;
        let scale = |v: u16| scale_to_16(v, depth);
        let ch = self.color_type.channels();
        let rb = self.row_bytes();
        for y in 0..h {
            let row = &self.data[y * rb..(y + 1) * rb];
            for x in 0..w {
                let s = |c: usize| sample_at(row, depth, x * ch + c);
                match self.color_type {
                    ColorType::Grayscale => {
                        let g = s(0);
                        let a = match self.transparency {
                            Some(Transparency::Gray(t)) if t == g => 0,
                            _ => 0xFFFF,
                        };
                        let g = scale(g);
                        out.extend_from_slice(&[g, g, g, a]);
                    }
                    ColorType::Rgb => {
                        let (r, g, b) = (s(0), s(1), s(2));
                        let a = match self.transparency {
                            Some(Transparency::Rgb(tr, tg, tb)) if (tr, tg, tb) == (r, g, b) => 0,
                            _ => 0xFFFF,
                        };
                        out.extend_from_slice(&[scale(r), scale(g), scale(b), a]);
                    }
                    ColorType::Indexed => {
                        let i = s(0) as usize;
                        let pal = self.palette.as_deref().unwrap_or(&[]);
                        let [r, g, b] = pal.get(i).copied().unwrap_or([0, 0, 0]);
                        let a = match &self.transparency {
                            Some(Transparency::Palette(t)) => t.get(i).copied().unwrap_or(255),
                            _ => 255,
                        };
                        out.extend_from_slice(&[
                            r as u16 * 257,
                            g as u16 * 257,
                            b as u16 * 257,
                            a as u16 * 257,
                        ]);
                    }
                    ColorType::GrayscaleAlpha => {
                        let g = scale(s(0));
                        out.extend_from_slice(&[g, g, g, scale(s(1))]);
                    }
                    ColorType::Rgba => {
                        out.extend_from_slice(&[scale(s(0)), scale(s(1)), scale(s(2)), scale(s(3))]);
                    }
                }
            }
        }
        out
    }

    /// The image as 8-bit RGBA, four bytes per pixel, as
    /// [`to_rgba16`](Image::to_rgba16) and then rounded to eight bits.
    pub fn to_rgba8(&self) -> Vec<u8> {
        if self.bit_depth <= 8 {
            // Exact: every value of to_rgba16 is then a multiple of 257.
            self.to_rgba16().into_iter().map(|v| (v >> 8) as u8).collect()
        } else {
            self.to_rgba16().into_iter().map(|v| ((v as u32 * 255 + 32895) >> 16) as u8).collect()
        }
    }
}

/// Bytes per row of an image of `width` pixels.
pub(crate) fn row_bytes(width: u32, color_type: ColorType, bit_depth: u8) -> usize {
    (width as usize * color_type.channels() * bit_depth as usize).div_ceil(8)
}

/// Sample number `i` of a packed row.
#[inline]
pub(crate) fn sample_at(row: &[u8], depth: u8, i: usize) -> u16 {
    match depth {
        16 => u16::from_be_bytes([row[2 * i], row[2 * i + 1]]),
        8 => row[i] as u16,
        d => {
            let bit = i * d as usize;
            let shift = 8 - d as usize - bit % 8;
            ((row[bit / 8] >> shift) & ((1u8 << d) - 1)) as u16
        }
    }
}

/// Scales a `depth`-bit sample to 16 bits by bit replication.
#[inline]
pub(crate) fn scale_to_16(v: u16, depth: u8) -> u16 {
    match depth {
        16 => v,
        8 => v * 257,
        d => {
            let mut out = 0u32;
            let mut filled = 0;
            while filled < 16 {
                out = (out << d) | v as u32;
                filled += d;
            }
            (out >> (filled - 16)) as u16
        }
    }
}

/// `cHRM`: CIE 1931 chromaticities of the white point and the primaries,
/// times 100000 (PNG §11.3.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chromaticities {
    /// White point x, y.
    pub white: (u32, u32),
    /// Red x, y.
    pub red: (u32, u32),
    /// Green x, y.
    pub green: (u32, u32),
    /// Blue x, y.
    pub blue: (u32, u32),
}

/// `cICP`: coding-independent code points (ITU-T H.273), PNG §11.3.2.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cicp {
    /// ColourPrimaries.
    pub color_primaries: u8,
    /// TransferCharacteristics.
    pub transfer_function: u8,
    /// MatrixCoefficients (PNG requires 0, RGB).
    pub matrix_coefficients: u8,
    /// VideoFullRangeFlag.
    pub full_range: bool,
}

/// `iCCP`: an embedded ICC profile (decompressed).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IccProfile {
    /// The profile name (Latin-1, 1-79 bytes).
    pub name: String,
    /// The profile itself.
    pub profile: Vec<u8>,
}

/// `mDCV`: mastering display colour volume (PNG §11.3.2.7), as stored:
/// chromaticities in units of 0.00002, luminances in 0.0001 cd/m².
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MasteringDisplay {
    /// Red, green and blue primaries, x and y.
    pub primaries: [(u16, u16); 3],
    /// White point x, y.
    pub white: (u16, u16),
    /// Maximum luminance.
    pub max_luminance: u32,
    /// Minimum luminance.
    pub min_luminance: u32,
}

/// `cLLI`: content light level information (PNG §11.3.2.8), in
/// 0.0001 cd/m².
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentLightLevel {
    /// Maximum content light level.
    pub max_cll: u32,
    /// Maximum frame-average light level.
    pub max_fall: u32,
}

/// How a text chunk was (or is to be) stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextKind {
    /// `tEXt`: Latin-1, uncompressed.
    Plain,
    /// `zTXt`: Latin-1, compressed.
    Compressed,
    /// `iTXt`: UTF-8, with a language tag and translated keyword; compressed
    /// or not.
    International {
        /// Whether the text is compressed.
        compressed: bool,
    },
}

/// A text chunk (`tEXt`, `zTXt` or `iTXt`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Text {
    /// The keyword (1-79 Latin-1 characters).
    pub keyword: String,
    /// The text.
    pub text: String,
    /// `iTXt`: the language tag (RFC 5646), possibly empty.
    pub language: String,
    /// `iTXt`: the keyword translated into the language, possibly empty.
    pub translated_keyword: String,
    /// The chunk type.
    pub kind: TextKind,
}

impl Text {
    /// A `tEXt` chunk.
    pub fn plain(keyword: &str, text: &str) -> Text {
        Text {
            keyword: keyword.into(),
            text: text.into(),
            language: String::new(),
            translated_keyword: String::new(),
            kind: TextKind::Plain,
        }
    }
}

/// `pHYs`: intended pixel size or aspect ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalDimensions {
    /// Pixels per unit, X axis.
    pub x: u32,
    /// Pixels per unit, Y axis.
    pub y: u32,
    /// Whether the unit is the metre (otherwise unknown: an aspect ratio).
    pub metre: bool,
}

/// `bKGD`: the background colour, at the image's depth (or a palette index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Background {
    /// Greyscale images.
    Gray(u16),
    /// Truecolour images.
    Rgb(u16, u16, u16),
    /// Indexed images: a palette index.
    Palette(u8),
}

/// `tIME`: last modification, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Time {
    /// Year (e.g. 1995).
    pub year: u16,
    /// Month 1-12.
    pub month: u8,
    /// Day 1-31.
    pub day: u8,
    /// Hour 0-23.
    pub hour: u8,
    /// Minute 0-59.
    pub minute: u8,
    /// Second 0-60.
    pub second: u8,
}

/// An ancillary chunk this crate does not interpret, kept as bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UnknownChunk {
    /// The chunk type.
    pub kind: [u8; 4],
    /// The chunk data.
    pub data: Vec<u8>,
}

/// Everything in a PNG besides the pixels and the animation. The decoder
/// fills in what the file has; the encoder writes what is set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Metadata {
    /// `gAMA`: image gamma times 100000.
    pub gamma: Option<u32>,
    /// `cHRM`.
    pub chromaticities: Option<Chromaticities>,
    /// `sRGB`: the rendering intent (0 perceptual, 1 relative colorimetric,
    /// 2 saturation, 3 absolute colorimetric).
    pub srgb: Option<u8>,
    /// `iCCP`.
    pub icc_profile: Option<IccProfile>,
    /// `cICP`.
    pub cicp: Option<Cicp>,
    /// `mDCV`.
    pub mastering_display: Option<MasteringDisplay>,
    /// `cLLI`.
    pub content_light_level: Option<ContentLightLevel>,
    /// `sBIT`: significant bits per channel (as many as the colour type has
    /// channels; three for indexed).
    pub significant_bits: Option<Vec<u8>>,
    /// `bKGD`.
    pub background: Option<Background>,
    /// `pHYs`.
    pub physical: Option<PhysicalDimensions>,
    /// `eXIf`: the Exif profile, as stored (TIFF header first).
    pub exif: Option<Vec<u8>>,
    /// `tIME`.
    pub time: Option<Time>,
    /// `tEXt`, `zTXt` and `iTXt`, in file order.
    pub text: Vec<Text>,
    /// Other ancillary chunks (`hIST`, `sPLT`, private chunks), in file
    /// order. The encoder writes them before the image data.
    pub unknown: Vec<UnknownChunk>,
}

/// `fcTL` dispose_op: what happens to a frame's region before the next
/// frame is drawn (APNG).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DisposeOp {
    /// Leave it.
    #[default]
    None,
    /// Clear it to fully transparent black.
    Background,
    /// Put back what was there before the frame was drawn.
    Previous,
}

/// `fcTL` blend_op: how a frame is drawn over the canvas (APNG).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BlendOp {
    /// Replace the region, alpha included.
    #[default]
    Source,
    /// Alpha-composite the frame over the region.
    Over,
}

/// An APNG frame's `fcTL` fields (the sequence number aside).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameControl {
    /// Frame width.
    pub width: u32,
    /// Frame height.
    pub height: u32,
    /// X position on the canvas.
    pub x_offset: u32,
    /// Y position on the canvas.
    pub y_offset: u32,
    /// Delay numerator: the frame shows for delay_num / delay_den seconds.
    pub delay_num: u16,
    /// Delay denominator (0 means 100).
    pub delay_den: u16,
    /// Dispose operation.
    pub dispose: DisposeOp,
    /// Blend operation.
    pub blend: BlendOp,
}

impl FrameControl {
    /// A frame covering the whole `width` x `height` canvas, no delay,
    /// dispose none, blend source.
    pub fn full(width: u32, height: u32) -> FrameControl {
        FrameControl {
            width,
            height,
            x_offset: 0,
            y_offset: 0,
            delay_num: 0,
            delay_den: 100,
            dispose: DisposeOp::None,
            blend: BlendOp::Source,
        }
    }
}

/// An APNG frame: its control fields and its pixels (same colour type, bit
/// depth, palette and transparency as the file's IHDR, `PLTE` and `tRNS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The `fcTL` fields.
    pub control: FrameControl,
    /// The frame's pixels, `control.width` x `control.height`.
    pub image: Image,
}

/// An APNG's animation (`acTL` and the frames).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Animation {
    /// Times to play; 0 is forever.
    pub num_plays: u32,
    /// Whether the static image (IDAT) is the first frame. When it is,
    /// `frames[0]` holds it; when not, it is shown only by decoders that do
    /// not animate and is not among `frames`.
    pub default_image_is_first_frame: bool,
    /// The frames, in order.
    pub frames: Vec<Frame>,
}
