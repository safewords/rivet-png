//! DEFLATE (RFC 1951) and zlib (RFC 1950), both directions, written from the
//! RFCs.
//!
//! This is the codec under PNG's image data, `zTXt`, `iTXt` and `iCCP`; it is
//! public so that other formats built on the same streams (TIFF's Deflate
//! compression, for one) can use it without a second implementation.
//!
//! ```
//! use rpng::deflate;
//!
//! let text = b"a rose is a rose is a rose is a rose".repeat(20);
//! let z = deflate::zlib_compress(&text, 6);
//! assert!(z.len() < text.len() / 4);
//! assert_eq!(deflate::zlib_decompress(&z).unwrap(), text);
//!
//! // A raw DEFLATE stream (no zlib header or Adler-32 trailer), forcing
//! // the block type.
//! let opts = deflate::Options { level: 9, block_type: deflate::BlockType::Fixed, ..Default::default() };
//! let raw = deflate::deflate_with(&text, &opts);
//! assert_eq!(deflate::inflate(&raw).unwrap(), text);
//! ```
//!
//! Decoding is one-shot over a byte slice. [`Inflater`] adds an output limit
//! (for untrusted streams that could expand without bound) and reports how
//! many input bytes the stream occupied.

mod checksum;
mod compress;
mod huffman;
mod inflate;
mod tables;

pub use checksum::{Crc32, adler32, adler32_update, crc32};
pub use compress::{BlockType, Options, deflate, deflate_with, zlib_compress, zlib_compress_with};
pub use inflate::{Inflated, Inflater, inflate, zlib_decompress};

/// A malformed DEFLATE or zlib stream.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The input ended before the final block did.
    #[error("the stream is truncated")]
    Truncated,
    /// The stream breaks RFC 1951 or RFC 1950: a reserved block type, a
    /// stored block whose length check fails, a Huffman code that is
    /// over-subscribed or lacks an end-of-block code, a distance that reaches
    /// back before the start of the output, a bad zlib header.
    #[error("{0}")]
    Invalid(&'static str),
    /// The zlib trailer's Adler-32 does not match the decompressed data.
    #[error("Adler-32 mismatch: the stream says {expected:08x}, the data is {actual:08x}")]
    Checksum {
        /// The trailer's value.
        expected: u32,
        /// The value computed over the output.
        actual: u32,
    },
    /// A zlib stream asks for a preset dictionary (FDICT), which neither PNG
    /// nor TIFF allows and this decoder does not take.
    #[error("the zlib stream needs a preset dictionary")]
    Dictionary,
    /// The output would exceed the limit given to [`Inflater::limit`].
    #[error("the decompressed data exceeds the {0}-byte limit")]
    Limit(usize),
}
