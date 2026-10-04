//! A PNG and APNG decoder and encoder, with its own DEFLATE and zlib.
//!
//! Written from the W3C PNG specification (third edition, which includes
//! APNG, `cICP`, `mDCV`, `cLLI` and `eXIf`), RFC 1950 and RFC 1951. No C,
//! no system libraries, no build script; one dependency (`thiserror`).
//!
//! ```
//! use rpng::{ColorType, Encoder, Image, FilterStrategy};
//!
//! // A 3x2 8-bit RGB image.
//! let px: Vec<u8> = (0..18).map(|i| i * 14).collect();
//! let image = Image::new(3, 2, ColorType::Rgb, 8, px).unwrap();
//! let png = Encoder { interlaced: true, filter: FilterStrategy::Adaptive, ..Encoder::with_level(9) }
//!     .encode(&image)
//!     .unwrap();
//! let back = rpng::decode(&png).unwrap();
//! assert_eq!(back.image, image);
//! assert!(back.interlaced);
//! assert_eq!(back.image.to_rgba8()[..4], [0, 14, 28, 255]);
//! ```
//!
//! - [`decode`] / [`Decoder`] read any valid PNG: every colour type and bit
//!   depth, palette and `tRNS`, Adam7, 16-bit samples, the colour-space
//!   chunks (reported, not applied), text, `eXIf`, and APNG frames
//!   ([`Animation::compose`] renders them with their dispose and blend
//!   operations). Chunk CRCs and the zlib Adler-32 are checked.
//! - [`Encoder`] writes the same: every colour type and bit depth, any of the
//!   five filters or adaptive selection per row, Adam7, compression levels
//!   0-9, metadata chunks, APNG ([`Encoder::encode_animation`]).
//! - [`deflate`] is the compression codec on its own, for other formats
//!   that use zlib or raw DEFLATE streams.
//!
//! Pixels are kept in PNG's own layout ([`Image`]); [`Image::to_rgba8`] and
//! [`Image::to_rgba16`] convert.

// Unsafe code is confined to the vector kernels in `simd`.
#![deny(unsafe_code)]
#![warn(missing_docs)]

mod decode;
pub mod deflate;
mod encode;
mod error;
mod filter;
mod interlace;
mod par;
mod simd;
mod types;

pub use decode::{ComposedFrame, Decoder, Header, Png, SIGNATURE, decode, read_header};
pub use encode::{Encoder, encode};
pub use error::{Error, Result};
pub use filter::{Filter, FilterStrategy};
pub use types::{
    Animation, Background, BlendOp, Chromaticities, Cicp, ColorType, ContentLightLevel, DisposeOp,
    Frame, FrameControl, IccProfile, Image, MasteringDisplay, Metadata, PhysicalDimensions, Text,
    TextKind, Time, Transparency, UnknownChunk,
};
