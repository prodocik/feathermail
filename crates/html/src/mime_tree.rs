//! Recursive `multipart/*` walking.
//!
//! Builds an in-memory tree of parts (headers + content-type + either raw
//! leaf bytes or child parts) without decoding any bodies yet — decoding
//! happens later, only for the part(s) actually selected (`select.rs`).

use crate::content_type::{ContentDisposition, ContentType};
use crate::headers::{split_headers, Headers};

/// Hard cap on multipart nesting depth. A message nested deeper than this
/// (accidentally or as a denial-of-service attempt) has its remaining
/// structure treated as one opaque leaf instead of being recursed into
/// further, so parsing can never stack-overflow on adversarial input.
pub(crate) const MAX_MULTIPART_DEPTH: usize = 32;

pub(crate) enum PartBody<'a> {
    /// A non-multipart part (or a multipart one we could not split, e.g. a
    /// missing/absent boundary): the raw, still transfer-encoded bytes.
    Leaf(&'a [u8]),
    /// A successfully split `multipart/*` part.
    Multipart(Vec<RawPart<'a>>),
    /// A `multipart/*` part beyond `MAX_MULTIPART_DEPTH`: left unparsed.
    DepthExceeded(&'a [u8]),
}

pub(crate) struct RawPart<'a> {
    pub headers: Headers,
    pub content_type: ContentType,
    pub disposition: Option<ContentDisposition>,
    pub body: PartBody<'a>,
}

fn trim_trailing_newline(seg: &[u8]) -> &[u8] {
    if let Some(rest) = seg.strip_suffix(b"\r\n") {
        rest
    } else if let Some(rest) = seg.strip_suffix(b"\n") {
        rest
    } else {
        seg
    }
}

fn strip_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((&b'\r', rest)) => rest,
        _ => line,
    }
}

/// Splits `body` on a MIME boundary per RFC 2046 §5.1. Returns the parts
/// found between (and including, leniently, after a missing terminator) the
/// delimiter lines. Returns an empty `Vec` if the boundary line never
/// occurs in `body` at all.
///
/// This is a single linear scan over lines: `pos` strictly increases by at
/// least one byte every loop iteration, so it always terminates, even when
/// the closing delimiter (`--boundary--`) is missing entirely.
fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Vec<&'a [u8]> {
    if boundary.is_empty() {
        return Vec::new();
    }
    let delim = format!("--{boundary}");
    let delim_bytes = delim.as_bytes();

    // (line_start, content_start, is_closing). `line_start` is where the
    // "--boundary..." line itself begins (used as the end boundary of the
    // *previous* segment, so that line is excluded from its content).
    // `content_start` is just past the line's terminator (used as the start
    // boundary of the segment that follows this delimiter).
    let mut marks: Vec<(usize, usize, bool)> = Vec::new();
    let mut pos = 0usize;
    loop {
        if pos >= body.len() {
            break;
        }
        let rest = &body[pos..];
        let nl = rest.iter().position(|&b| b == b'\n');
        let (line, line_len) = match nl {
            Some(i) => (&rest[..i], i + 1),
            None => (rest, rest.len()),
        };
        let content = strip_cr(line);
        if let Some(after) = content.strip_prefix(delim_bytes) {
            // RFC 2046 §5.1.1 allows `transport-padding := *LWSP-char`
            // between the boundary (or its closing `--`) and the CRLF.
            // Without trimming it, a `--boundary \r\n` line written by a
            // standards-following sender matches nothing, `marks` stays
            // empty and the whole multipart body is served as one opaque
            // leaf — i.e. an empty message plus a bogus attachment.
            // Only SP/HTAB are trimmed: `--boundaryXYZ` must stay a
            // non-delimiter, or foreign text could cut the body apart.
            let mut after = after;
            while let [head @ .., b' ' | b'\t'] = after {
                after = head;
            }
            if after.is_empty() {
                marks.push((pos, pos + line_len, false));
            } else if after == b"--" {
                marks.push((pos, pos, true));
            }
        }
        if nl.is_none() {
            break;
        }
        pos += line_len;
    }

    if marks.is_empty() {
        return Vec::new();
    }

    let mut parts = Vec::new();
    for i in 0..marks.len() {
        let (_, seg_start, is_closing) = marks[i];
        if is_closing {
            break; // nothing after the closing delimiter belongs to a part
        }
        let seg_end = marks
            .get(i + 1)
            .map(|&(line_start, _, _)| line_start)
            .unwrap_or(body.len());
        let raw_seg = body.get(seg_start..seg_end).unwrap_or(&[]);
        parts.push(trim_trailing_newline(raw_seg));
    }
    parts
}

/// Parses `raw` (headers + body, as a single MIME part or an entire
/// message) into a `RawPart` tree. `depth` is the multipart nesting level;
/// callers parsing a whole message start at `depth = 0`.
pub(crate) fn parse_part(raw: &[u8], depth: usize) -> RawPart<'_> {
    let (headers, body) = split_headers(raw);
    let content_type = headers
        .get_first("Content-Type")
        .map(ContentType::parse)
        .unwrap_or_default();
    let disposition = headers
        .get_first("Content-Disposition")
        .map(ContentDisposition::parse);

    if content_type.is_multipart() {
        if depth >= MAX_MULTIPART_DEPTH {
            return RawPart {
                headers,
                content_type,
                disposition,
                body: PartBody::DepthExceeded(body),
            };
        }
        if let Some(boundary) = content_type.boundary() {
            let segments = split_multipart(body, boundary);
            if !segments.is_empty() {
                let children: Vec<RawPart> = segments
                    .into_iter()
                    .map(|seg| parse_part(seg, depth + 1))
                    .collect();
                return RawPart {
                    headers,
                    content_type,
                    disposition,
                    body: PartBody::Multipart(children),
                };
            }
        }
    }

    RawPart {
        headers,
        content_type,
        disposition,
        body: PartBody::Leaf(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf_bytes<'a>(part: &RawPart<'a>) -> &'a [u8] {
        match part.body {
            PartBody::Leaf(b) => b,
            _ => panic!("expected leaf"),
        }
    }

    #[test]
    fn simple_multipart_two_parts() {
        let raw = b"Content-Type: multipart/mixed; boundary=X\r\n\r\n--X\r\nContent-Type: text/plain\r\n\r\nfirst\r\n--X\r\nContent-Type: text/html\r\n\r\nsecond\r\n--X--\r\n";
        let part = parse_part(raw, 0);
        match part.body {
            PartBody::Multipart(children) => {
                assert_eq!(children.len(), 2);
                assert_eq!(leaf_bytes(&children[0]), b"first");
                assert_eq!(leaf_bytes(&children[1]), b"second");
            }
            _ => panic!("expected multipart"),
        }
    }

    #[test]
    fn missing_boundary_in_body_yields_no_children() {
        let raw =
            b"Content-Type: multipart/mixed; boundary=X\r\n\r\nno boundary markers here at all\r\n";
        let part = parse_part(raw, 0);
        match part.body {
            PartBody::Leaf(_) => {} // treated as an opaque, unsplittable leaf
            _ => panic!("expected leaf fallback"),
        }
    }

    #[test]
    fn missing_closing_delimiter_still_yields_last_part_no_infinite_loop() {
        let raw = b"Content-Type: multipart/mixed; boundary=X\r\n\r\n--X\r\nContent-Type: text/plain\r\n\r\nonly part, never closed\r\nmore text\r\n";
        let part = parse_part(raw, 0);
        match part.body {
            PartBody::Multipart(children) => {
                assert_eq!(children.len(), 1);
                assert!(leaf_bytes(&children[0]).starts_with(b"only part"));
            }
            _ => panic!("expected multipart"),
        }
    }

    #[test]
    fn nested_multipart_is_walked() {
        let inner = b"--Y\r\nContent-Type: text/plain\r\n\r\ninner text\r\n--Y--\r\n";
        let raw = format!(
            "Content-Type: multipart/mixed; boundary=X\r\n\r\n--X\r\nContent-Type: multipart/alternative; boundary=Y\r\n\r\n{}--X--\r\n",
            String::from_utf8_lossy(inner)
        );
        let part = parse_part(raw.as_bytes(), 0);
        match part.body {
            PartBody::Multipart(children) => {
                assert_eq!(children.len(), 1);
                assert!(children[0].content_type.is("multipart", "alternative"));
                match &children[0].body {
                    PartBody::Multipart(grandchildren) => {
                        assert_eq!(grandchildren.len(), 1);
                        assert_eq!(leaf_bytes(&grandchildren[0]), b"inner text");
                    }
                    _ => panic!("expected nested multipart"),
                }
            }
            _ => panic!("expected multipart"),
        }
    }

    #[test]
    fn depth_beyond_limit_does_not_recurse_or_panic() {
        // Build a message nested MAX_MULTIPART_DEPTH + 5 levels deep.
        let mut body = "leaf text".to_string();
        for i in 0..(MAX_MULTIPART_DEPTH + 5) {
            let boundary = format!("B{i}");
            body = format!(
                "Content-Type: multipart/mixed; boundary={boundary}\r\n\r\n--{boundary}\r\n{body}\r\n--{boundary}--\r\n"
            );
        }
        let part = parse_part(body.as_bytes(), 0);
        // Must not panic (stack overflow) and must terminate; walk down and
        // confirm we hit a DepthExceeded leaf before the nesting count.
        fn walk(part: &RawPart, levels: &mut usize) -> bool {
            *levels += 1;
            match &part.body {
                PartBody::DepthExceeded(_) => true,
                PartBody::Leaf(_) => false,
                PartBody::Multipart(children) => children.iter().any(|c| walk(c, levels)),
            }
        }
        let mut levels = 0usize;
        let hit_limit = walk(&part, &mut levels);
        assert!(
            hit_limit,
            "expected to hit MAX_MULTIPART_DEPTH before running out of structure"
        );
        assert!(levels <= MAX_MULTIPART_DEPTH + 2);
    }

    #[test]
    fn empty_boundary_param_does_not_panic() {
        let raw = b"Content-Type: multipart/mixed; boundary=\r\n\r\nsomething\r\n";
        let part = parse_part(raw, 0);
        assert!(matches!(part.body, PartBody::Leaf(_)));
    }

    #[test]
    fn boundary_with_transport_padding_is_recognised() {
        let raw = b"Content-Type: multipart/mixed; boundary=X\r\n\r\n--X \t\r\nContent-Type: text/plain\r\n\r\nfirst\r\n--X\t\r\nContent-Type: text/html\r\n\r\nsecond\r\n--X-- \r\n";
        let part = parse_part(raw, 0);
        match part.body {
            PartBody::Multipart(children) => {
                assert_eq!(children.len(), 2, "expected two parts");
                assert_eq!(leaf_bytes(&children[0]), b"first");
                assert_eq!(leaf_bytes(&children[1]), b"second");
            }
            _ => panic!("expected multipart, boundary with transport padding was not recognised"),
        }
    }

    #[test]
    fn a_boundary_line_with_a_non_lwsp_suffix_is_not_a_delimiter() {
        // Only SP/HTAB is transport-padding. If any trailing text were
        // tolerated, a sender could cut a message apart with a line of
        // their own prose that merely starts like the boundary.
        let raw = b"Content-Type: multipart/mixed; boundary=X\r\n\r\n--X\r\nContent-Type: text/plain\r\n\r\nfirst\r\n--Xtrailing\r\nstill first\r\n--X--\r\n";
        let part = parse_part(raw, 0);
        match part.body {
            PartBody::Multipart(children) => {
                assert_eq!(children.len(), 1, "--Xtrailing must not split the part");
                assert_eq!(
                    leaf_bytes(&children[0]),
                    b"first\r\n--Xtrailing\r\nstill first"
                );
            }
            _ => panic!("expected multipart"),
        }
    }
}
