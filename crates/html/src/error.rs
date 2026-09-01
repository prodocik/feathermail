//! Decode-failure error type.
//!
//! D14: message content never appears in logs or error text. `Display` on
//! `DecodeError` is a fixed, static string per variant — it never borrows or
//! copies from the input bytes being parsed. See `tests::display_never_contains_input`
//! in `select.rs` for the regression test that pins this down.

use std::fmt;

/// A message part could not be turned into displayable text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// `Content-Transfer-Encoding` named something other than the handful of
    /// tokens this crate understands (`base64`, `quoted-printable`, `7bit`,
    /// `8bit`, `binary`). We have no general decoder to fall back to, so the
    /// part is reported as undecodable rather than shown as garbage.
    UnknownTransferEncoding,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnknownTransferEncoding => {
                write!(
                    f,
                    "message part could not be decoded: unrecognized transfer encoding"
                )
            }
        }
    }
}

impl std::error::Error for DecodeError {}
