//! Turning a parsed MIME tree into "what the UI shows": one body (plain
//! text or HTML) plus a list of attachments. Decoding (transfer-encoding +
//! charset) only happens for parts actually chosen — rejected
//! `multipart/alternative` siblings are never decoded.

use crate::content_type::ContentType;
use crate::error::DecodeError;
use crate::mime_tree::{parse_part, PartBody, RawPart};
use crate::transfer::{decode_base64, decode_quoted_printable};
use std::collections::HashMap;

/// HTML exactly as it appeared in the message, **not sanitized**.
///
/// This type exists so an unsanitized HTML string cannot be handed to a
/// renderer by accident through a bare `String`. Sanitizing it (dropping
/// `<script>`, event-handler attributes, remote image loads, etc.) is done
/// by `crate::sanitize::sanitize` (T-030). Do not pass the contents of this
/// type to WebKitGTK, or any other HTML renderer, without running it
/// through that sanitizer first.
#[derive(Clone, PartialEq, Eq)]
pub struct UnsanitizedHtml(String);

impl UnsanitizedHtml {
    fn new(html: String) -> Self {
        UnsanitizedHtml(html)
    }

    /// The raw, unsanitized HTML. See the type-level docs before using this.
    pub fn as_unsanitized_str(&self) -> &str {
        &self.0
    }

    /// Consumes `self` and returns the raw, unsanitized HTML. See the
    /// type-level docs before using this.
    pub fn into_unsanitized_string(self) -> String {
        self.0
    }

    /// Test-only constructor for `crate::sanitize`'s tests (a different
    /// module, so it cannot reach the private tuple field directly).
    /// Production code always gets an `UnsanitizedHtml` from
    /// `parse_message`, never builds one from a literal.
    #[cfg(test)]
    pub(crate) fn for_test(html: String) -> Self {
        UnsanitizedHtml(html)
    }
}

/// D14: `Debug` on message content must never leak the content. Manual
/// impl instead of `derive` — the derive would print the wrapped `String`
/// verbatim, defeating the whole point of this newtype. See
/// `crate::sanitize::tests::sanitized_html_debug_never_contains_body_or_attribute_content`
/// for the sibling test on the sanitized side; the regression test for
/// this one is `tests::unsanitized_html_debug_never_contains_content` below.
impl std::fmt::Debug for UnsanitizedHtml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnsanitizedHtml")
            .field("len", &self.0.len())
            .finish()
    }
}

/// The message body chosen for display.
#[derive(Clone, PartialEq, Eq)]
pub enum BodyContent {
    Plain(String),
    Html(UnsanitizedHtml),
    /// No displayable body part was found (e.g. the message is only
    /// attachments, or is empty).
    Empty,
    /// A body part was found but could not be decoded (see `DecodeError`).
    Undecodable(DecodeError),
}

/// Transfer encoding declared by an attachment MIME part. This is metadata,
/// not a decoder choice made by a caller: the download path uses it to
/// decode exactly the section the MIME tree identified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentTransferEncoding {
    Base64,
    QuotedPrintable,
    Identity,
    Unsupported,
}

/// D14: `Debug` on message content must never leak the content, same
/// reasoning as [`UnsanitizedHtml`]'s manual impl just above -- a derived
/// `Debug` here would print `Plain`'s wrapped `String` verbatim. Manual
/// impl instead of `derive`; each variant prints its name and (where it
/// carries text) a length, never the text itself. `Html`'s payload is
/// already covered by `UnsanitizedHtml`'s own manual `Debug`, and
/// `Undecodable`'s payload is [`DecodeError`], whose `Debug` is a fixed
/// per-variant string that never borrows from message bytes (see that
/// type's doc comment) -- both safe to print as-is. See
/// `tests::body_content_debug_never_contains_plain_text_content` for the
/// regression test.
impl std::fmt::Debug for BodyContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyContent::Plain(text) => f
                .debug_tuple("Plain")
                .field(&format_args!("<{} bytes>", text.len()))
                .finish(),
            BodyContent::Html(html) => f.debug_tuple("Html").field(html).finish(),
            BodyContent::Empty => f.write_str("Empty"),
            BodyContent::Undecodable(err) => f.debug_tuple("Undecodable").field(err).finish(),
        }
    }
}

/// One attachment: enough for the UI to list it. Carries no bytes and does
/// no I/O — writing attachment contents to disk is a different crate's job
/// (`crates/attachments`).
#[derive(Clone, PartialEq, Eq)]
pub struct AttachmentInfo {
    pub name: Option<String>,
    /// Normalized Content-ID without the RFC angle brackets. This is the
    /// key an HTML `cid:` reference uses; it is metadata only and is never
    /// interpreted as a URL outside the bounded inline-image resolver.
    pub content_id: Option<String>,
    /// `"type/subtype"`, lowercased.
    pub content_type: String,
    /// Size in bytes *after* transfer-decoding (base64/quoted-printable),
    /// i.e. the size the actual file content would be — computed in memory
    /// and then discarded, never written anywhere.
    pub size_bytes: usize,
    /// IMAP body section containing just this part's payload: `1`, `2.1`,
    /// and so on. A non-multipart attachment is `TEXT`, never `BODY[]`, so
    /// fetching it does not include the enclosing RFC822 headers.
    pub section: String,
    /// Declared content-transfer-encoding, normalized into the only modes
    /// the streaming attachment cache knows how to decode.
    pub transfer_encoding: AttachmentTransferEncoding,
}

/// D14: `name` is message content -- a filename like
/// "Договор_Иванов_паспорт.pdf" says as much about a person as a subject
/// line does, and `ParsedMessage`'s own manual `Debug` (see below) prints
/// this type as-is through its `attachments` field, so a derived `Debug`
/// here would undo that protection through a field nobody was looking at.
/// `content_type` and `size_bytes` are not message content -- they are
/// MIME/transport metadata this crate already treats as safe to surface
/// (they are the exact fields `crates/core` copies into the searchable
/// `attachment_names`/size accounting), so they print as-is, same as
/// `UnsanitizedHtml`/`BodyContent`/`ParsedMessage` above. See
/// `tests::attachment_info_debug_never_contains_the_filename`.
impl std::fmt::Debug for AttachmentInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentInfo")
            .field("name_bytes", &self.name.as_ref().map(|n| n.len()))
            .field("has_content_id", &self.content_id.is_some())
            .field("content_type", &self.content_type)
            .field("size_bytes", &self.size_bytes)
            .field("section", &self.section)
            .field("transfer_encoding", &self.transfer_encoding)
            .finish()
    }
}

fn is_attachment_leaf(part: &RawPart) -> bool {
    matches!(&part.disposition, Some(d) if d.is_attachment())
}

fn transfer_decode(cte: &str, raw: &[u8]) -> Option<Vec<u8>> {
    match cte {
        "base64" => Some(decode_base64(raw)),
        "quoted-printable" => Some(decode_quoted_printable(raw)),
        "7bit" | "8bit" | "binary" | "" => Some(raw.to_vec()),
        _ => None,
    }
}

fn attachment_transfer_encoding(cte: &str) -> AttachmentTransferEncoding {
    match cte {
        "base64" => AttachmentTransferEncoding::Base64,
        "quoted-printable" => AttachmentTransferEncoding::QuotedPrintable,
        "7bit" | "8bit" | "binary" | "" => AttachmentTransferEncoding::Identity,
        _ => AttachmentTransferEncoding::Unsupported,
    }
}

fn content_transfer_encoding(part: &RawPart) -> String {
    part.headers
        .get_first("Content-Transfer-Encoding")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

fn make_attachment_info(part: &RawPart, section: &str) -> AttachmentInfo {
    let name = part
        .disposition
        .as_ref()
        .and_then(|d| d.filename())
        .or_else(|| part.content_type.name())
        .map(crate::rfc2047::decode_encoded_words);

    let raw = match &part.body {
        PartBody::Leaf(b) | PartBody::DepthExceeded(b) => *b,
        PartBody::Multipart(_) => &[][..],
    };
    let cte = content_transfer_encoding(part);
    let transfer_encoding = attachment_transfer_encoding(&cte);
    let size_bytes = decoded_size(transfer_encoding, raw);

    AttachmentInfo {
        name,
        content_id: normalized_content_id(part.headers.get_first("Content-ID")),
        content_type: part.content_type.full(),
        size_bytes,
        section: if section.is_empty() {
            "TEXT".to_string()
        } else {
            section.to_string()
        },
        transfer_encoding,
    }
}

fn normalized_content_id(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    let value = value
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
        .unwrap_or(value)
        .trim();
    (!value.is_empty()).then(|| value.to_ascii_lowercase())
}

/// Decode only CID-addressable raster parts that are small enough to hand
/// to the isolated renderer. The regular attachment path stays streaming;
/// this deliberately bounded exception exists solely for images embedded
/// in the letter itself.
pub(crate) fn inline_image_data(
    raw_message: &[u8],
    max_image_bytes: usize,
    max_total_bytes: usize,
) -> HashMap<String, (String, Vec<u8>)> {
    fn collect(
        part: &RawPart<'_>,
        output: &mut HashMap<String, (String, Vec<u8>)>,
        used: &mut usize,
        max_image_bytes: usize,
        max_total_bytes: usize,
    ) {
        match &part.body {
            PartBody::Multipart(children) => {
                for child in children {
                    collect(child, output, used, max_image_bytes, max_total_bytes);
                }
            }
            PartBody::Leaf(raw) => {
                let Some(content_id) = normalized_content_id(part.headers.get_first("Content-ID"))
                else {
                    return;
                };
                let content_type = part.content_type.full();
                if !matches!(
                    content_type.as_str(),
                    "image/png" | "image/jpeg" | "image/gif" | "image/webp"
                ) {
                    return;
                }
                let encoding = attachment_transfer_encoding(&content_transfer_encoding(part));
                if encoding == AttachmentTransferEncoding::Unsupported {
                    return;
                }
                let expected = decoded_size(encoding, raw);
                if expected == 0
                    || expected > max_image_bytes
                    || used.saturating_add(expected) > max_total_bytes
                {
                    return;
                }
                let Some(bytes) = transfer_decode(&content_transfer_encoding(part), raw) else {
                    return;
                };
                if bytes.len() > max_image_bytes
                    || used.saturating_add(bytes.len()) > max_total_bytes
                    || !has_image_signature(&content_type, &bytes)
                {
                    return;
                }
                *used += bytes.len();
                output.entry(content_id).or_insert((content_type, bytes));
            }
            PartBody::DepthExceeded(_) => {}
        }
    }

    let root = parse_part(raw_message, 0);
    let mut output = HashMap::new();
    let mut used = 0usize;
    collect(
        &root,
        &mut output,
        &mut used,
        max_image_bytes,
        max_total_bytes,
    );
    output
}

fn has_image_signature(content_type: &str, bytes: &[u8]) -> bool {
    match content_type {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => {
            bytes.starts_with(b"RIFF") && bytes.get(8..12).is_some_and(|tag| tag == b"WEBP")
        }
        _ => false,
    }
}

/// Counts decoded bytes without allocating a second copy of an attachment
/// merely to populate the metadata list. The logic intentionally mirrors the
/// parser's tolerant decoders: garbage is skipped for base64 and malformed
/// quoted-printable escapes drop only their `=` marker.
fn decoded_size(encoding: AttachmentTransferEncoding, raw: &[u8]) -> usize {
    match encoding {
        AttachmentTransferEncoding::Base64 => {
            let chars = raw
                .iter()
                .filter(
                    |byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/'),
                )
                .count();
            chars / 4 * 3 + chars % 4 * 6 / 8
        }
        AttachmentTransferEncoding::QuotedPrintable => {
            let mut total = 0usize;
            let mut i = 0usize;
            while i < raw.len() {
                if raw[i] != b'=' {
                    total += 1;
                    i += 1;
                } else if raw.get(i + 1) == Some(&b'\r') && raw.get(i + 2) == Some(&b'\n') {
                    i += 3;
                } else if raw.get(i + 1) == Some(&b'\n') {
                    i += 2;
                } else if raw
                    .get(i + 1)
                    .zip(raw.get(i + 2))
                    .is_some_and(|(&high, &low)| {
                        hex_value(high).is_some() && hex_value(low).is_some()
                    })
                {
                    total += 1;
                    i += 3;
                } else {
                    i += 1;
                }
            }
            total
        }
        AttachmentTransferEncoding::Identity | AttachmentTransferEncoding::Unsupported => raw.len(),
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The "effective" content type of `part` for `multipart/alternative`
/// selection purposes, without decoding any bytes: what type would this
/// part resolve to if it were chosen as the body? `None` means "not a
/// displayable text part" (an attachment, a non-text leaf, or a subtree
/// with nothing displayable in it).
fn effective_type<'p, 'a>(part: &'p RawPart<'a>, prefer_plain: bool) -> Option<&'p ContentType> {
    match &part.body {
        PartBody::Leaf(_) => {
            if part.content_type.is_multipart() || is_attachment_leaf(part) {
                None
            } else if part.content_type.is("text", "plain") || part.content_type.is("text", "html")
            {
                Some(&part.content_type)
            } else {
                None
            }
        }
        PartBody::DepthExceeded(_) => None,
        PartBody::Multipart(children) => {
            if part.content_type.is("multipart", "alternative") {
                choose_alternative(children, prefer_plain)
                    .and_then(|c| effective_type(c, prefer_plain))
            } else {
                children
                    .iter()
                    .filter(|c| !is_attachment_leaf(c))
                    .find_map(|c| effective_type(c, prefer_plain))
            }
        }
    }
}

/// Picks the child of a `multipart/alternative` to display: the preferred
/// type (`text/plain` if `prefer_plain`, else `text/html`) if present, else
/// the other text type, else whichever child resolves to any displayable
/// type at all.
fn choose_alternative<'p, 'a>(
    children: &'p [RawPart<'a>],
    prefer_plain: bool,
) -> Option<&'p RawPart<'a>> {
    let (want_t, want_s) = if prefer_plain {
        ("text", "plain")
    } else {
        ("text", "html")
    };
    let (alt_t, alt_s) = if prefer_plain {
        ("text", "html")
    } else {
        ("text", "plain")
    };
    let matches = |c: &&RawPart<'a>, t: &str, s: &str| {
        effective_type(c, prefer_plain)
            .map(|ct| ct.is(t, s))
            .unwrap_or(false)
    };
    children
        .iter()
        .find(|c| matches(c, want_t, want_s))
        .or_else(|| children.iter().find(|c| matches(c, alt_t, alt_s)))
        .or_else(|| {
            children
                .iter()
                .find(|c| effective_type(c, prefer_plain).is_some())
        })
}

/// Walks `part`, returning the leaf chosen as the message body (if any) and
/// pushing every attachment found (in `part`'s own subtree) into
/// `attachments`. Recursion depth is already bounded by
/// `mime_tree::MAX_MULTIPART_DEPTH` at parse time, so this cannot overflow
/// the stack no matter how deeply nested the input claimed to be.
fn find_body_leaf<'p, 'a>(
    part: &'p RawPart<'a>,
    section: &str,
    prefer_plain: bool,
    attachments: &mut Vec<AttachmentInfo>,
) -> Option<(&'p RawPart<'a>, String)> {
    match &part.body {
        PartBody::Leaf(_) => {
            if part.content_type.is_multipart() {
                // Declared multipart/* but we couldn't split it (missing or
                // absent boundary): not usable as a body, but the bytes are
                // real content the message claimed to carry — invariant:
                // every leaf is either the body or an attachment, so list
                // the part itself rather than silently discarding it.
                attachments.push(make_attachment_info(part, section));
                None
            } else if is_attachment_leaf(part) {
                attachments.push(make_attachment_info(part, section));
                None
            } else if part.content_type.is("text", "plain") || part.content_type.is("text", "html")
            {
                Some((part, section.to_string()))
            } else {
                // Not text, not explicitly marked as an attachment: still
                // not something we can show as the body (covers images or
                // documents an old client attached without a
                // Content-Disposition header at all).
                attachments.push(make_attachment_info(part, section));
                None
            }
        }
        PartBody::DepthExceeded(_) => {
            // Too deep to safely recurse into: surface it as an
            // attachment-like stub instead of silently dropping it.
            attachments.push(make_attachment_info(part, section));
            None
        }
        PartBody::Multipart(children) => {
            if part.content_type.is("multipart", "alternative") {
                // Alternative siblings that *are* valid representations of
                // the same content (another text/plain or text/html) are
                // genuinely just dropped when unchosen — that is what
                // "alternative" means, not a leak. But a child that is not
                // a valid alternative at all (non-text, or explicitly
                // marked as an attachment) is not an alternative rendering
                // of anything — walk it too, so it still ends up in
                // `attachments` instead of vanishing when no alternative
                // was chosen, or when the chosen one wasn't this child.
                let chosen = choose_alternative(children, prefer_plain);
                let mut result = None;
                for (index, child) in children.iter().enumerate() {
                    let child_section = child_section(section, index + 1);
                    let is_chosen = chosen.is_some_and(|c| std::ptr::eq(c, child));
                    if is_chosen {
                        result = find_body_leaf(child, &child_section, prefer_plain, attachments);
                    } else if effective_type(child, prefer_plain).is_none() {
                        // Not a valid alternative representation: recurse
                        // as if collecting attachments from a plain
                        // subtree, and if it still resolves to a
                        // body-shaped leaf (e.g. a nested multipart with a
                        // stray text part inside it), list that leaf as an
                        // attachment rather than dropping it.
                        if let Some((leaf, leaf_section)) =
                            find_body_leaf(child, &child_section, prefer_plain, attachments)
                        {
                            attachments.push(make_attachment_info(leaf, &leaf_section));
                        }
                    }
                    // else: a valid-but-unchosen text alternative — an
                    // alternate rendering of the same content, intentionally
                    // not listed as an attachment.
                }
                result
            } else {
                // multipart/mixed, multipart/related, multipart/report, or
                // any other multipart/* subtype: the first non-attachment
                // child that resolves to a body is the body; every child is
                // still walked (for attachment collection), and any
                // *further* body-shaped leaf found after the first is
                // listed as an attachment rather than silently dropped.
                let mut body_leaf = None;
                for (index, child) in children.iter().enumerate() {
                    let child_section = child_section(section, index + 1);
                    let found = find_body_leaf(child, &child_section, prefer_plain, attachments);
                    match (body_leaf.is_some(), found) {
                        (false, Some(leaf)) => body_leaf = Some(leaf),
                        (true, Some((extra, extra_section))) => {
                            attachments.push(make_attachment_info(extra, &extra_section))
                        }
                        _ => {}
                    }
                }
                body_leaf
            }
        }
    }
}

fn child_section(parent: &str, index: usize) -> String {
    if parent.is_empty() {
        index.to_string()
    } else {
        format!("{parent}.{index}")
    }
}

fn decode_leaf_body(leaf: &RawPart) -> BodyContent {
    let raw = match &leaf.body {
        PartBody::Leaf(b) => *b,
        _ => return BodyContent::Empty,
    };
    let cte = content_transfer_encoding(leaf);
    let decoded_bytes = match transfer_decode(&cte, raw) {
        Some(b) => b,
        None => return BodyContent::Undecodable(DecodeError::UnknownTransferEncoding),
    };
    // RFC 2045 default charset is us-ascii; here we default to utf-8
    // instead (see charset::decode_bytes docs for the full unknown-charset
    // rationale, which applies equally to "no charset given" — most
    // unlabeled mail in the wild is ASCII-compatible UTF-8, and this
    // degrades to the same output as us-ascii whenever the bytes actually
    // are ASCII).
    let charset = leaf.content_type.charset().unwrap_or("utf-8");
    let text = crate::charset::decode_bytes(charset, &decoded_bytes);
    if leaf.content_type.is("text", "html") {
        BodyContent::Html(UnsanitizedHtml::new(text))
    } else {
        BodyContent::Plain(text)
    }
}

pub(crate) fn select(root: &RawPart, prefer_plain: bool) -> (BodyContent, Vec<AttachmentInfo>) {
    let mut attachments = Vec::new();
    let leaf = find_body_leaf(root, "", prefer_plain, &mut attachments);
    let body = leaf
        .map(|(leaf, _section)| decode_leaf_body(leaf))
        .unwrap_or(BodyContent::Empty);
    (body, attachments)
}

/// A parsed message: headers, decoded subject, the chosen body, and the
/// list of attachments (names/types/sizes only, no bytes).
pub struct ParsedMessage {
    pub headers: crate::headers::Headers,
    /// RFC 2047-decoded `Subject`, if present.
    pub subject: Option<String>,
    pub body: BodyContent,
    pub attachments: Vec<AttachmentInfo>,
}

/// D14: same reasoning as [`BodyContent`]'s manual `Debug` just above --
/// this type carries message content (`subject`, and every header value
/// including `Subject`/`From`/`To`/`Received` text through `headers`) that
/// a derived `Debug` would print verbatim. `headers` prints as a count
/// (`Headers::len`), never `{:?}` on the header list itself; `subject`
/// prints `Some(<N bytes>)`/`None`, never the decoded text; `body` reuses
/// [`BodyContent`]'s own safe `Debug`; `attachments` reuses
/// [`AttachmentInfo`]'s own safe `Debug` -- the filename is message
/// content and must not print verbatim there either (a filename like
/// "Договор_Иванов_паспорт.pdf" says as much about a person as a subject
/// line does), while `content_type`/`size_bytes` are transport metadata
/// this crate already treats as safe to surface (they are the exact
/// fields `crates/core` copies into the searchable `attachment_names` FTS
/// column), not body content.
/// See `tests::parsed_message_debug_never_contains_subject_header_or_body_content`.
impl std::fmt::Debug for ParsedMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParsedMessage")
            .field("header_count", &self.headers.len())
            .field("subject_bytes", &self.subject.as_ref().map(|s| s.len()))
            .field("body", &self.body)
            .field("attachments", &self.attachments)
            .finish()
    }
}

/// Parses raw RFC 822/5322 message bytes into a `ParsedMessage`.
///
/// `prefer_plain` selects `text/plain` over `text/html` when a
/// `multipart/alternative` offers both; when the preferred type isn't
/// present, whatever alternative *is* available is used instead.
///
/// Never panics on any input, including empty input, headers with no body,
/// bodies with no headers, and arbitrary/random bytes.
pub fn parse_message(raw: &[u8], prefer_plain: bool) -> ParsedMessage {
    let root = parse_part(raw, 0);
    let subject = root.headers.get_decoded("Subject");
    let (body, attachments) = select(&root, prefer_plain);
    ParsedMessage {
        headers: root.headers,
        subject,
        body,
        attachments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(msg: &ParsedMessage) -> &str {
        match &msg.body {
            BodyContent::Plain(s) => s,
            other => panic!("expected Plain body, got {other:?}"),
        }
    }

    fn html(msg: &ParsedMessage) -> &str {
        match &msg.body {
            BodyContent::Html(h) => h.as_unsanitized_str(),
            other => panic!("expected Html body, got {other:?}"),
        }
    }

    #[test]
    fn unsanitized_html_debug_never_contains_content() {
        let canary = "SECRET-D14-MARKER";
        let raw = format!("Content-Type: text/html; charset=utf-8\r\n\r\n<p>{canary}</p>");
        let msg = parse_message(raw.as_bytes(), false);
        let h = match &msg.body {
            BodyContent::Html(h) => h,
            other => panic!("expected Html body, got {other:?}"),
        };
        assert!(h.as_unsanitized_str().contains(canary), "sanity check");
        let debugged = format!("{h:?}");
        assert!(
            !debugged.contains(canary),
            "Debug leaked message content: {debugged}"
        );
    }

    #[test]
    fn body_content_debug_never_contains_plain_text_content() {
        let canary = "SECRET-D14-PLAIN-MARKER";
        let raw = format!("Content-Type: text/plain; charset=utf-8\r\n\r\n{canary}");
        let msg = parse_message(raw.as_bytes(), true);
        assert_eq!(plain(&msg), canary, "sanity check");
        let debugged = format!("{:?}", msg.body);
        assert!(
            !debugged.contains(canary),
            "Debug leaked message content: {debugged}"
        );
    }

    #[test]
    fn parsed_message_debug_never_contains_subject_header_or_body_content() {
        // T-093 review finding: `ParsedMessage`/`BodyContent` derived
        // `Debug` -- unlike `UnsanitizedHtml`, which already got a manual
        // impl in T-030 for the same reason. One marker per leak surface
        // this type touches: the decoded `Subject`, an arbitrary header
        // (`Received`, chosen because `crates/core`'s T-093 test already
        // treats it as the canonical "this is header text, not body text"
        // example), and the selected plain-text body.
        let subject_canary = "SECRET-D14-SUBJECT-MARKER";
        let header_canary = "SECRET-D14-HEADER-MARKER";
        let body_canary = "SECRET-D14-BODY-MARKER";
        let raw = format!(
            "Subject: {subject_canary}\r\nReceived: from mail.example.com ({header_canary})\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body_canary}"
        );
        let msg = parse_message(raw.as_bytes(), true);
        // Sanity checks: the markers really are reachable through the
        // safe, non-`Debug` accessors first, so the assertion below can't
        // pass just because parsing silently dropped them.
        assert_eq!(msg.subject.as_deref(), Some(subject_canary));
        assert_eq!(
            msg.headers
                .get_first("Received")
                .map(|v| v.contains(header_canary)),
            Some(true)
        );
        assert_eq!(plain(&msg), body_canary);

        let debugged = format!("{msg:?}");
        assert!(
            !debugged.contains(subject_canary),
            "Debug leaked the subject: {debugged}"
        );
        assert!(
            !debugged.contains(header_canary),
            "Debug leaked a header value: {debugged}"
        );
        assert!(
            !debugged.contains(body_canary),
            "Debug leaked the body: {debugged}"
        );
    }

    #[test]
    fn attachment_info_debug_never_contains_the_filename() {
        // T-093 review finding (round 3): `ParsedMessage::fmt` delegates
        // `attachments` to `AttachmentInfo`'s own `Debug`, but that type
        // was still on `#[derive(Debug)]`, so the filename -- message
        // content exactly like the subject, since e.g. a real filename
        // can name a person -- printed verbatim through a field nobody
        // was looking at when `BodyContent`/`ParsedMessage` were fixed.
        // Real multipart/mixed message, attachment with a marker filename.
        let filename_canary = "SECRET-D14-FILENAME.pdf";
        let raw = format!(
            "Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nbody text\r\n--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"{filename_canary}\"\r\n\r\n%PDF\r\n--B--\r\n"
        );
        let msg = parse_message(raw.as_bytes(), true);
        assert_eq!(
            msg.attachments.len(),
            1,
            "sanity check: exactly one attachment parsed"
        );
        assert_eq!(
            msg.attachments[0].name.as_deref(),
            Some(filename_canary),
            "sanity check: the filename really is reachable through the safe, non-`Debug` accessor first"
        );

        let attachment_debugged = format!("{:?}", msg.attachments[0]);
        assert!(
            !attachment_debugged.contains(filename_canary),
            "AttachmentInfo::fmt leaked the filename: {attachment_debugged}"
        );

        let message_debugged = format!("{msg:?}");
        assert!(
            !message_debugged.contains(filename_canary),
            "ParsedMessage::fmt leaked the filename through attachments: {message_debugged}"
        );
    }

    #[test]
    fn plain_single_part_message() {
        let raw = b"Subject: hi\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nhello there";
        let msg = parse_message(raw, true);
        assert_eq!(msg.subject.as_deref(), Some("hi"));
        assert_eq!(plain(&msg), "hello there");
        assert!(msg.attachments.is_empty());
    }

    #[test]
    fn message_with_no_headers_at_all_does_not_panic() {
        let msg = parse_message(b"just some plain text, no headers", true);
        assert!(msg.headers.is_empty());
        // No Content-Type, so default text/plain applies to the whole body.
        assert_eq!(plain(&msg), "just some plain text, no headers");
    }

    #[test]
    fn empty_input_does_not_panic() {
        let msg = parse_message(b"", true);
        assert!(msg.headers.is_empty());
        // No Content-Type header means the RFC 2045 default (text/plain)
        // applies, and the body is a zero-length text/plain part.
        assert_eq!(msg.body, BodyContent::Plain(String::new()));
        assert!(msg.attachments.is_empty());
    }

    #[test]
    fn body_of_only_newlines() {
        // One "\r\n" ends the header line, one ends the (empty) header
        // block; the rest is body, kept verbatim (this parser does not trim
        // trailing whitespace from a body it did not have to split out of a
        // multipart boundary).
        let raw = b"Content-Type: text/plain\r\n\r\n\r\n\r\n\r\n";
        let msg = parse_message(raw, true);
        assert_eq!(plain(&msg), "\r\n\r\n\r\n");
    }

    #[test]
    fn multipart_alternative_prefers_plain_when_asked() {
        let raw = b"Content-Type: multipart/alternative; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nplain body\r\n--B\r\nContent-Type: text/html\r\n\r\n<p>html body</p>\r\n--B--\r\n";
        let msg = parse_message(raw, true);
        assert_eq!(plain(&msg), "plain body");
    }

    #[test]
    fn multipart_alternative_prefers_html_when_asked() {
        let raw = b"Content-Type: multipart/alternative; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nplain body\r\n--B\r\nContent-Type: text/html\r\n\r\n<p>html body</p>\r\n--B--\r\n";
        let msg = parse_message(raw, false);
        assert_eq!(html(&msg), "<p>html body</p>");
    }

    #[test]
    fn multipart_alternative_falls_back_when_preferred_missing() {
        let raw = b"Content-Type: multipart/alternative; boundary=B\r\n\r\n--B\r\nContent-Type: text/html\r\n\r\n<p>only html</p>\r\n--B--\r\n";
        let msg = parse_message(raw, true); // prefers plain, but only html exists
        assert_eq!(html(&msg), "<p>only html</p>");
    }

    #[test]
    fn attachment_is_excluded_from_body_and_listed() {
        let raw = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nbody text\r\n--B\r\nContent-Type: application/pdf; name=report.pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--B--\r\n";
        let msg = parse_message(raw, true);
        assert_eq!(plain(&msg), "body text");
        assert_eq!(msg.attachments.len(), 1);
        let att = &msg.attachments[0];
        assert_eq!(att.name.as_deref(), Some("report.pdf"));
        assert_eq!(att.content_type, "application/pdf");
        assert_eq!(att.size_bytes, 5); // "hello" decoded from base64
        assert_eq!(att.section, "2");
        assert_eq!(att.transfer_encoding, AttachmentTransferEncoding::Base64);
    }

    #[test]
    fn single_part_attachment_uses_text_section_without_copying_its_payload() {
        let raw = b"Content-Type: application/pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\n\r\n%PDF";
        let msg = parse_message(raw, true);
        let att = msg.attachments.first().expect("one attachment");
        assert_eq!(att.section, "TEXT");
        assert_eq!(att.transfer_encoding, AttachmentTransferEncoding::Identity);
        assert_eq!(att.size_bytes, 4);
    }

    #[test]
    fn text_plain_attachment_is_not_merged_into_body() {
        // A text/plain part explicitly marked as an attachment must stay out
        // of the body and appear only in the attachment list, even though
        // its content-type alone would otherwise make it look like a body
        // candidate.
        let raw = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nreal body\r\n--B\r\nContent-Type: text/plain; name=notes.txt\r\nContent-Disposition: attachment; filename=notes.txt\r\n\r\nattached notes\r\n--B--\r\n";
        let msg = parse_message(raw, true);
        assert_eq!(plain(&msg), "real body");
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].name.as_deref(), Some("notes.txt"));
        assert_eq!(msg.attachments[0].size_bytes, "attached notes".len());
    }

    /// Recursively counts every `Leaf`/`DepthExceeded` node in the parsed
    /// tree. Each one must end up as either the chosen body or an
    /// attachment — see `assert_no_leaf_lost`.
    fn count_leaves(part: &RawPart) -> usize {
        match &part.body {
            PartBody::Leaf(_) | PartBody::DepthExceeded(_) => 1,
            PartBody::Multipart(children) => children.iter().map(count_leaves).sum(),
        }
    }

    /// Invariant: no leaf of the parsed MIME tree disappears without a
    /// trace. Every leaf is either the chosen body (any `BodyContent` other
    /// than `Empty`, counted as 1), listed in `attachments`, or a *valid,
    /// unchosen* `multipart/alternative` sibling (another representation of
    /// the same content in a different format — dropping those is correct,
    /// RFC-mandated behavior, not a leak, so callers state how many of
    /// those they expect via `expected_dropped_alternatives`). This is
    /// checked structurally (by counting) rather than against one fixed
    /// example, so a future rearrangement of the selection logic can't
    /// quietly reopen the hole without failing a test.
    fn assert_leaf_accounting(
        raw: &[u8],
        prefer_plain: bool,
        expected_dropped_alternatives: usize,
    ) {
        let root = crate::mime_tree::parse_part(raw, 0);
        let total_leaves = count_leaves(&root);
        let msg = parse_message(raw, prefer_plain);
        let body_count = usize::from(!matches!(msg.body, BodyContent::Empty));
        let attachment_count = msg.attachments.len();
        assert_eq!(
            total_leaves,
            body_count + attachment_count + expected_dropped_alternatives,
            "leaf accounting mismatch: {total_leaves} leaves in the tree, \
             but body_count={body_count} + attachments={attachment_count} + \
             expected_dropped_alternatives={expected_dropped_alternatives}"
        );
    }

    /// Shorthand for the common case: nothing at all should have been
    /// legitimately dropped, so every leaf must be the body or an
    /// attachment.
    fn assert_no_leaf_lost(raw: &[u8], prefer_plain: bool) {
        assert_leaf_accounting(raw, prefer_plain, 0);
    }

    #[test]
    fn alternative_with_no_resolvable_child_lists_both_as_attachments() {
        // Regression for a review finding: neither alternative child is
        // text, and one is explicitly marked as an attachment. Before this
        // fix, `choose_alternative` returned None, so *nothing* under this
        // multipart/alternative was ever walked — both leaves vanished
        // (body Empty, attachments empty), silently losing a real PDF the
        // message actually contained. Now: body stays Empty (nothing here
        // can be shown as text), but both children are surfaced as
        // attachments instead of disappearing.
        let raw = b"Content-Type: multipart/alternative; boundary=ALT\r\n\r\n--ALT\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"a.pdf\"\r\n\r\n%PDF\r\n--ALT\r\nContent-Type: image/png\r\n\r\nPNG\r\n--ALT--\r\n";
        let msg = parse_message(raw, true);
        assert_eq!(msg.body, BodyContent::Empty);
        assert_eq!(msg.attachments.len(), 2);
        assert!(msg
            .attachments
            .iter()
            .any(|a| a.content_type == "application/pdf"));
        assert!(msg
            .attachments
            .iter()
            .any(|a| a.content_type == "image/png"));
        assert_no_leaf_lost(raw, true);
    }

    #[test]
    fn unsplittable_declared_multipart_is_listed_as_attachment_not_dropped() {
        // A part declared multipart/* whose boundary parameter never
        // actually occurs as a delimiter line in the body cannot be split
        // into children (mime_tree falls back to treating it as one opaque
        // leaf) — but the bytes are still real content the message claimed
        // to carry, and used to vanish silently here too.
        let raw = b"Content-Type: multipart/related; boundary=NEVER-APPEARS\r\n\r\nthis body has no NEVER-APPEARS boundary line at all\r\n";
        let msg = parse_message(raw, true);
        assert_eq!(msg.body, BodyContent::Empty);
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].content_type, "multipart/related");
        assert_no_leaf_lost(raw, true);
    }

    #[test]
    fn leaf_invariant_holds_across_many_shapes() {
        // The leaf-accounting invariant checked across a spread of tree
        // shapes (not just one hand-picked example), per review: a plain
        // single part, empty input, headerless input, multipart/alternative
        // in both preference directions, a mixed message with a
        // non-text attachment, a mixed message with a text/plain part
        // explicitly marked as an attachment, the two regressions fixed
        // above, and a random-bytes case.
        let zero_drop_fixtures: &[(&[u8], bool)] = &[
            (
                b"Subject: hi\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nhello there",
                true,
            ),
            (b"", true),
            (b"just some plain text, no headers", true),
            (
                b"Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nbody text\r\n--B\r\nContent-Type: application/pdf; name=report.pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--B--\r\n",
                true,
            ),
            (
                b"Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nreal body\r\n--B\r\nContent-Type: text/plain; name=notes.txt\r\nContent-Disposition: attachment; filename=notes.txt\r\n\r\nattached notes\r\n--B--\r\n",
                true,
            ),
            (
                b"Content-Type: multipart/alternative; boundary=ALT\r\n\r\n--ALT\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=\"a.pdf\"\r\n\r\n%PDF\r\n--ALT\r\nContent-Type: image/png\r\n\r\nPNG\r\n--ALT--\r\n",
                true,
            ),
            (
                b"Content-Type: multipart/related; boundary=NEVER-APPEARS\r\n\r\nthis body has no NEVER-APPEARS boundary line at all\r\n",
                true,
            ),
        ];
        for (raw, prefer_plain) in zero_drop_fixtures {
            assert_no_leaf_lost(raw, *prefer_plain);
        }

        // multipart/alternative with two *valid* children (text/plain and
        // text/html): whichever one isn't chosen is legitimately dropped —
        // it is the same content in another format, not a lost attachment.
        // Checked in both preference directions.
        let two_valid_alternative = b"Content-Type: multipart/alternative; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nplain body\r\n--B\r\nContent-Type: text/html\r\n\r\n<p>html body</p>\r\n--B--\r\n";
        assert_leaf_accounting(two_valid_alternative, true, 1);
        assert_leaf_accounting(two_valid_alternative, false, 1);
    }

    #[test]
    fn solo_attachment_marked_part_yields_empty_body_not_its_content() {
        // A single, non-multipart message whose only part is explicitly
        // marked Content-Disposition: attachment must never be shown as the
        // body just because its content-type happens to be text/plain: the
        // body must be Empty, and the content must show up only in the
        // attachment list. Unlike the sibling-based test above, there is no
        // second body candidate here, so this isolates the
        // disposition-attachment exclusion itself (as opposed to the
        // generic "a second body-shaped leaf found after the first becomes
        // an attachment" fallback, which alone cannot produce Empty here).
        let raw = b"Content-Type: text/plain\r\nContent-Disposition: attachment; filename=only.txt\r\n\r\njust an attachment";
        let msg = parse_message(raw, true);
        assert_eq!(msg.body, BodyContent::Empty);
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].name.as_deref(), Some("only.txt"));
        assert_eq!(msg.attachments[0].size_bytes, "just an attachment".len());
    }

    #[test]
    fn nested_multipart_related_inside_mixed() {
        let raw = concat!(
            "Content-Type: multipart/mixed; boundary=OUTER\r\n\r\n",
            "--OUTER\r\n",
            "Content-Type: multipart/related; boundary=INNER\r\n\r\n",
            "--INNER\r\n",
            "Content-Type: text/html\r\n\r\n<p>hi</p>\r\n",
            "--INNER\r\n",
            "Content-Type: image/png\r\nContent-ID: <img1>\r\n\r\nnotreallypng\r\n",
            "--INNER--\r\n",
            "--OUTER\r\n",
            "Content-Type: application/zip\r\nContent-Disposition: attachment; filename=x.zip\r\n\r\nzipbytes\r\n",
            "--OUTER--\r\n",
        );
        let msg = parse_message(raw.as_bytes(), true);
        assert_eq!(html(&msg), "<p>hi</p>");
        // Inline related image + the explicit attachment.
        assert_eq!(msg.attachments.len(), 2);
        assert!(msg
            .attachments
            .iter()
            .any(|a| a.content_type == "image/png"));
        assert!(msg
            .attachments
            .iter()
            .any(|a| a.content_id.as_deref() == Some("img1")));
        assert!(msg
            .attachments
            .iter()
            .any(|a| a.content_type == "application/zip"));
    }

    #[test]
    fn base64_body_decodes() {
        let raw = b"Content-Type: text/plain\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8gd29ybGQ=\r\n";
        let msg = parse_message(raw, true);
        assert_eq!(plain(&msg), "hello world");
    }

    #[test]
    fn quoted_printable_body_decodes() {
        let raw = "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\ncaf=C3=A9\r\n".as_bytes();
        let msg = parse_message(raw, true);
        // The trailing CRLF from the raw message is part of the body verbatim.
        assert_eq!(plain(&msg), "café\r\n");
    }

    #[test]
    fn unknown_transfer_encoding_is_undecodable_not_a_panic() {
        let raw = b"Content-Type: text/plain\r\nContent-Transfer-Encoding: x-uuencode\r\n\r\nwhatever\r\n";
        let msg = parse_message(raw, true);
        assert!(matches!(
            msg.body,
            BodyContent::Undecodable(DecodeError::UnknownTransferEncoding)
        ));
    }

    #[test]
    fn windows_1251_subject_and_body_decode() {
        // "Привет" cp1251-encoded, RFC2047 in Subject, raw bytes in body.
        let cp1251_privet: &[u8] = &[0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]; // "Привет"
        let mut raw = Vec::new();
        raw.extend_from_slice(b"Subject: =?windows-1251?Q?=CF=F0=E8=E2=E5=F2?=\r\n");
        raw.extend_from_slice(b"Content-Type: text/plain; charset=windows-1251\r\n\r\n");
        raw.extend_from_slice(cp1251_privet);
        let msg = parse_message(&raw, true);
        assert_eq!(msg.subject.as_deref(), Some("Привет"));
        assert_eq!(plain(&msg), "Привет");
    }

    #[test]
    fn charset_claims_utf8_but_bytes_are_invalid_does_not_panic() {
        let mut raw = b"Content-Type: text/plain; charset=utf-8\r\n\r\n".to_vec();
        raw.extend_from_slice(&[0xFF, 0xFE, 0xFD]);
        let msg = parse_message(&raw, true);
        // Must produce *a* string, not panic.
        let _ = plain(&msg);
    }

    #[test]
    fn decode_error_display_never_contains_input_bytes() {
        let canary = "TOTALLY-SECRET-CANARY-VALUE-XYZ";
        let raw = format!(
            "Content-Type: text/plain\r\nContent-Transfer-Encoding: x-mystery\r\n\r\n{canary}\r\n"
        );
        let msg = parse_message(raw.as_bytes(), true);
        let err = match msg.body {
            BodyContent::Undecodable(e) => e,
            other => panic!("expected Undecodable, got {other:?}"),
        };
        let rendered = err.to_string();
        assert!(
            !rendered.contains(canary),
            "error text leaked message content: {rendered}"
        );
    }

    #[test]
    fn decode_error_display_and_debug_never_contain_body_or_encoding_token() {
        // Addressed regression, per review: the previous D14 test only
        // checked that *a* canary from the body was absent from `Display`.
        // A header can leak just as easily as a body, and `Debug` output
        // (not just `Display`) ends up in logs more often than people
        // expect. Plant two distinct, recognizable markers — one in the
        // body, one as the (unknown) `Content-Transfer-Encoding` token
        // itself — and check both `Display` and `Debug` of the resulting
        // error against both markers.
        let body_canary = "BODY-CANARY-4f9c1e2a";
        let encoding_canary = "x-encoding-canary-7b3d";
        let raw = format!(
            "Content-Type: text/plain
Content-Transfer-Encoding: {encoding_canary}

{body_canary}
"
        );
        let msg = parse_message(raw.as_bytes(), true);
        let err = match msg.body {
            BodyContent::Undecodable(e) => e,
            other => panic!("expected Undecodable, got {other:?}"),
        };
        let displayed = err.to_string();
        let debugged = format!("{err:?}");
        for (label, rendered) in [("Display", &displayed), ("Debug", &debugged)] {
            assert!(
                !rendered.contains(body_canary),
                "{label} leaked the body: {rendered}"
            );
            assert!(
                !rendered.contains(encoding_canary),
                "{label} leaked the Content-Transfer-Encoding token: {rendered}"
            );
        }
    }

    #[test]
    fn fuzz_random_bytes_never_panics() {
        // Deterministic pseudo-random bytes (no external RNG dependency
        // allowed in this crate): a simple xorshift-style generator is
        // enough to exercise the parser on unstructured input.
        let mut state: u64 = 0x243F6A8885A308D3;
        let mut buf = Vec::with_capacity(8192);
        for _ in 0..8192 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            buf.push((state & 0xFF) as u8);
        }
        for prefer_plain in [true, false] {
            let msg = parse_message(&buf, prefer_plain);
            // Just must not panic; touch the fields so nothing is optimized away.
            let _ = (&msg.headers, &msg.subject, &msg.body, &msg.attachments);
            assert_no_leaf_lost(&buf, prefer_plain);
        }
    }

    #[test]
    fn structural_fuzz_mutated_nested_multipart_never_panics() {
        // `fuzz_random_bytes_never_panics` above gates on unstructured
        // bytes, which almost never form a real `--boundary` line — so it
        // barely touches `split_multipart`, the riskiest code in this
        // crate. This test instead starts from one valid, deeply nested
        // message (multipart/mixed containing a multipart/alternative with
        // a quoted-printable windows-1251 part and a base64 utf-8 part,
        // plus a sibling base64 attachment) and applies four kinds of
        // damage at pseudo-random positions, many times over: truncation,
        // single-byte flips, chunk deletion, and garbage insertion — the
        // shapes real corruption (bad IMAP fetch, truncated cache file,
        // manual editing) actually takes.
        let base = concat!(
            "Content-Type: multipart/mixed; boundary=OUTER\r\n\r\n",
            "--OUTER\r\n",
            "Content-Type: multipart/alternative; boundary=ALT\r\n\r\n",
            "--ALT\r\n",
            "Content-Type: text/plain; charset=windows-1251\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n\r\n",
            "=CF=F0=E8=E2=E5=F2\r\n",
            "--ALT\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "PHA+aGk8L3A+\r\n",
            "--ALT--\r\n",
            "--OUTER\r\n",
            "Content-Type: application/pdf\r\n",
            "Content-Disposition: attachment; filename=report.pdf\r\n",
            "Content-Transfer-Encoding: base64\r\n\r\n",
            "JVBERi0xLjQgZmFrZSBwZGYgYnl0ZXM=\r\n",
            "--OUTER--\r\n",
        )
        .as_bytes();

        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next_u64 = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..20_000 {
            let r = next_u64();
            let mut buf = base.to_vec();
            match r % 4 {
                0 => {
                    // Truncate at a random position.
                    let cut = (r as usize) % (buf.len() + 1);
                    buf.truncate(cut);
                }
                1 => {
                    // Flip a single random byte.
                    let i = (r as usize) % buf.len();
                    buf[i] ^= ((r >> 8) & 0xFF) as u8;
                }
                2 => {
                    // Delete a random chunk.
                    let a = (r as usize) % buf.len();
                    let chunk_len = 1 + ((r >> 16) as usize) % 32;
                    let b = (a + chunk_len).min(buf.len());
                    buf.drain(a..b);
                }
                _ => {
                    // Insert random garbage bytes.
                    let i = (r as usize) % (buf.len() + 1);
                    let garbage_len = 1 + ((r >> 16) as usize) % 32;
                    let garbage: Vec<u8> = (0..garbage_len)
                        .map(|k| ((r >> (k % 7 * 8)) & 0xFF) as u8)
                        .collect();
                    buf.splice(i..i, garbage);
                }
            }
            for prefer_plain in [true, false] {
                let msg = parse_message(&buf, prefer_plain);
                // Must not panic; touch the fields so nothing is optimized
                // away. Damage can legitimately break the leaf-accounting
                // invariant in ways that are hard to characterize in
                // general (e.g. truncating mid-header can turn a real
                // attachment into nothing at all) — that invariant is
                // covered on well-formed input elsewhere in this module;
                // this test's job is strictly "never panics" under damage.
                let _ = (&msg.headers, &msg.subject, &msg.body, &msg.attachments);
            }
        }
    }

    #[test]
    fn degenerate_boundaries_do_not_panic() {
        // Boundary values that are individually pathological rather than
        // randomly damaged: an empty boundary parameter, a boundary that is
        // just a dash (collides with the "--" delimiter prefix itself), a
        // multipart body with no closing delimiter at all, and a delimiter
        // line built from repeated "--A--A--A--A" (which could confuse a
        // naive scanner for the closing "--boundary--" marker).
        let fixtures: &[&[u8]] = &[
            b"Content-Type: multipart/mixed; boundary=\"\"\r\n\r\nsome body\r\n",
            b"Content-Type: multipart/mixed; boundary=-\r\n\r\n---\r\nContent-Type: text/plain\r\n\r\nx\r\n----\r\n",
            b"Content-Type: multipart/mixed; boundary=X\r\n\r\n--X\r\nContent-Type: text/plain\r\n\r\nnever closed, no trailing delimiter",
            b"Content-Type: multipart/mixed; boundary=A\r\n\r\n--A--A--A--A\r\nContent-Type: text/plain\r\n\r\nweird delimiter line\r\n--A--\r\n",
        ];
        for raw in fixtures {
            for prefer_plain in [true, false] {
                let msg = parse_message(raw, prefer_plain);
                let _ = (&msg.headers, &msg.subject, &msg.body, &msg.attachments);
            }
        }
    }

    #[test]
    fn depth_limited_attachment_is_still_listed_not_silently_dropped() {
        let mut body = "leaf".to_string();
        for i in 0..(crate::mime_tree::MAX_MULTIPART_DEPTH + 3) {
            let b = format!("B{i}");
            body = format!(
                "Content-Type: multipart/mixed; boundary={b}\r\n\r\n--{b}\r\n{body}\r\n--{b}--\r\n"
            );
        }
        let msg = parse_message(body.as_bytes(), true);
        assert_eq!(msg.body, BodyContent::Empty);
        assert_eq!(msg.attachments.len(), 1);
    }
}
