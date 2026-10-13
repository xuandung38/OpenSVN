//! Pure, I/O-free svndiff codec for the OpenSVN engine.
//!
//! * [`SvndiffDecoder`] turns arbitrary byte chunks (svndiff 0, 1 or 2) into
//!   [`svn_types::TxDeltaWindow`]s.
//! * [`WindowApplier`] is a [`svn_types::WindowSink`] that applies windows onto a source stream
//!   and writes the target while hashing it (MD5 and SHA-1).
//! * [`SvndiffEncoder`] writes full-text svndiff 0/1/2 streams.
//! * [`Base64Decoder`] / [`Base64SvndiffDecoder`] unwrap svndiff embedded in XML.
//!
//! Every malformed input is reported as an [`svn_types::SvnError`] with
//! [`svn_types::ErrorCode::Corrupt`]; nothing here panics on hostile data and all section sizes
//! are bounded before allocation.

mod apply;
mod base64;
mod compress;
mod decoder;
mod encoder;
mod varint;
mod window;

pub use apply::{ApplyHandle, ApplyOutcome, WindowApplier};
pub use base64::{Base64Decoder, Base64SvndiffDecoder};
pub use decoder::SvndiffDecoder;
pub use encoder::{SvndiffEncoder, SvndiffVersion, WINDOW_SIZE};
pub use varint::{decode_varint, encode_varint};
pub use window::{parse_instructions, serialize_instructions, validate_window};
