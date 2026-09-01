//! MIME parsing for raw RFC 822/5322 mail (T-031, first half).
//!
//! Pure library, no I/O: takes `&[u8]` of a raw message and returns a
//! parsed representation the UI reads from. Nothing here talks to the
//! filesystem or the network, and `unsafe_code = "forbid"` is enforced at
//! the workspace level.
//!
//! **HTML is sanitized here, rendered in `crates/app`.** [`UnsanitizedHtml`]
//! carries whatever HTML the message contained, unmodified. [`sanitize`] is
//! the only way to turn it into [`SanitizedHtml`]. The GTK shell (T-030
//! second half, `crates/app/src/html_view.rs`) is the only allowed
//! renderer: it runs sanitize off the GTK thread and loads the result
//! into a WebKitGTK widget with JS/popups/`file://` off.
//!
//! Robustness is half the point of this crate: real mail violates every
//! standard it claims to follow, so nothing here panics — not on missing
//! headers, not on a boundary that never appears, not on truncated
//! base64/quoted-printable, not on a charset that lies about its own bytes,
//! not on multipart nesting deep enough to blow the stack (capped, see
//! `mime_tree::MAX_MULTIPART_DEPTH`), and not on arbitrary random bytes
//! (see the fuzz test in `select.rs`).
//!
//! **HTML sanitizing (T-030, first half)** lives in `sanitize.rs`, built on
//! `ammonia`/`html5ever` — the one deliberate exception to "no external
//! dependencies" (see `docs/plan.md`, T-030, for why a security boundary is
//! the right place to spend that). [`UnsanitizedHtml`] still never leaves
//! this crate in raw form; [`sanitize`] is the only way to turn it into
//! [`SanitizedHtml`], which is the only HTML type safe to hand to a
//! renderer.

mod charset;
mod content_type;
mod css;
mod error;
mod headers;
mod hidden;
mod mime_tree;
mod prescan;
mod rfc2047;
mod sanitize;
mod select;
mod text;
mod tracking;
mod transfer;

pub use content_type::{ContentDisposition, ContentType};
pub use error::DecodeError;
pub use headers::Headers;
pub use rfc2047::decode_encoded_words;
pub use sanitize::{
    sanitize, sanitize_message_html, SanitizeOptions, SanitizeReport, SanitizedHtml,
    DEFAULT_MAX_INLINE_IMAGES_BYTES, DEFAULT_MAX_INLINE_IMAGE_BYTES, DEFAULT_MAX_INPUT_BYTES,
};
pub use select::{
    parse_message, AttachmentInfo, AttachmentTransferEncoding, BodyContent, ParsedMessage,
    UnsanitizedHtml,
};
pub use text::text_for_search;

/// Workspace probe so `cargo test -p feathermail-html` always has at least
/// one trivially-passing test independent of the parser itself.
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::crate_name;

    #[test]
    fn crate_compiles() {
        assert!(crate_name().starts_with("feathermail-"));
    }
}
