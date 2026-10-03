//! The crate's one error type.

use crate::deflate;

/// What went wrong. Every malformed input comes back as one of these; the
/// decoder never panics on bytes it is given.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The file breaks the PNG specification: a bad signature, a chunk CRC
    /// that does not match, a chunk out of order or of the wrong length, a
    /// field out of range, image data that runs short.
    #[error("invalid PNG data: {0}")]
    Invalid(String),
    /// The zlib / DEFLATE stream inside the file (image data, a compressed
    /// text chunk, an ICC profile) is malformed.
    #[error("invalid compressed data: {0}")]
    Deflate(#[from] deflate::Error),
    /// Valid PNG this crate does not implement, named: an unknown critical
    /// chunk, a compression or filter method other than 0.
    #[error("unsupported PNG feature: {0}")]
    Unsupported(String),
    /// What the caller asked the encoder for cannot be written: a colour type
    /// and bit depth PNG does not allow, a buffer of the wrong size, a palette
    /// missing for an indexed image, APNG frames that do not fit the canvas.
    #[error("invalid PNG configuration: {0}")]
    Config(String),
    /// The image is larger than the decoder's configured limit.
    #[error("image exceeds the decoder's limit: {0}")]
    Limit(String),
}

pub(crate) fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

pub(crate) fn config(msg: impl Into<String>) -> Error {
    Error::Config(msg.into())
}

/// `std::result::Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
