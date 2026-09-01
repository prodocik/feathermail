//! RFC 5322 header parsing: split raw message bytes into a header list and a
//! body, unfolding continuation lines along the way.
//!
//! Everything here works by scanning for `b'\n'` and slicing with checked
//! (`get`) or index-tracked accesses that only ever move forward by at least
//! one byte per line, so it always terminates and never panics on malformed
//! or truncated input.

/// A parsed header list. Names are matched case-insensitively (RFC 5322
/// field names are case-insensitive); original casing and order are kept,
/// and repeated headers (e.g. multiple `Received:`) are all retained.
#[derive(Clone, Default)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

/// D14: header *values* are message content -- `Subject`, `From`, `To`,
/// `Received` (which can carry internal hostnames/IPs) all live here, and
/// this was the one leak surface `crates/html`'s D14 pass had not yet
/// closed (`ParsedMessage`'s own `Debug` only ever printed `headers.len()`,
/// never delegated to this type's `Debug`, which is exactly why nothing in
/// the tree had noticed this derive was unsafe -- not because it was safe).
/// Header *names* are not content -- `Subject`/`From`/`Received` etc. are a
/// fixed, small vocabulary defined by RFC 5322, not user data -- so they
/// print as-is; only the paired value is redacted to a length. See
/// `tests::headers_debug_never_contains_a_header_value`.
impl std::fmt::Debug for Headers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Headers")
            .field("count", &self.entries.len())
            .field(
                "names",
                &self
                    .entries
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Headers {
    fn new() -> Self {
        Headers {
            entries: Vec::new(),
        }
    }

    /// First header value matching `name` (case-insensitive), if any.
    pub fn get_first(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// All header values matching `name` (case-insensitive), in order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.entries
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// All header (name, value) pairs, in original order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// First header value matching `name`, RFC 2047 decoded (`=?charset?B?..?=`).
    /// Use for human-facing header fields such as `Subject`, `From`, `To`.
    pub fn get_decoded(&self, name: &str) -> Option<String> {
        self.get_first(name)
            .map(crate::rfc2047::decode_encoded_words)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Strips a single trailing `\r` from a line slice, if present.
fn strip_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((&b'\r', rest)) => rest,
        _ => line,
    }
}

/// A line does not look like `Name: value` if it has no `:`, or the part
/// before `:` is empty or contains whitespace (RFC 5322 field names are a
/// single token, no spaces). This is a heuristic, not a strict grammar check:
/// plain-text bodies that happen to contain `word: word` on their very first
/// line will be misread as a one-header message. That is an inherent
/// ambiguity of "headers end at the first thing that doesn't parse as a
/// header", not something this parser can resolve perfectly; it only needs
/// to never panic, which it doesn't.
fn split_name_value(line: &[u8]) -> Option<(String, String)> {
    let colon = line.iter().position(|&b| b == b':')?;
    let name_bytes = &line[..colon];
    if name_bytes.is_empty() || name_bytes.iter().any(|&b| b == b' ' || b == b'\t') {
        return None;
    }
    let mut value_bytes = &line[colon + 1..];
    // Trim leading spaces/tabs from the value (conventional "Name: value").
    while let Some((&first, rest)) = value_bytes.split_first() {
        if first == b' ' || first == b'\t' {
            value_bytes = rest;
        } else {
            break;
        }
    }
    let name = String::from_utf8_lossy(name_bytes).into_owned();
    let value = String::from_utf8_lossy(value_bytes).into_owned();
    Some((name, value))
}

/// Splits `raw` into headers and body per RFC 5322 §2.1 (headers end at the
/// first blank line) with folded values unfolded per §2.2.3 (a folded value
/// is rejoined by dropping the CRLF and keeping the continuation line's
/// leading whitespace as-is).
///
/// Never panics: the loop advances `pos` by at least the length of one line
/// each iteration and stops as soon as `pos` reaches `raw.len()`.
pub fn split_headers(raw: &[u8]) -> (Headers, &[u8]) {
    let mut headers = Headers::new();
    let mut pending: Option<(String, String)> = None;
    let mut pos = 0usize;

    loop {
        if pos >= raw.len() {
            if let Some(h) = pending.take() {
                headers.entries.push(h);
            }
            break;
        }
        let rest = &raw[pos..];
        let nl = rest.iter().position(|&b| b == b'\n');
        let (line, next_pos) = match nl {
            Some(i) => (&rest[..i], pos + i + 1),
            None => (rest, raw.len()),
        };
        let content = strip_cr(line);

        if content.is_empty() {
            // Blank line: end of headers.
            if let Some(h) = pending.take() {
                headers.entries.push(h);
            }
            pos = next_pos;
            break;
        }

        let is_continuation = matches!(content.first(), Some(b' ') | Some(b'\t'));
        if is_continuation && pending.is_some() {
            if let Some((_, v)) = pending.as_mut() {
                v.push_str(&String::from_utf8_lossy(content));
            }
        } else {
            if let Some(h) = pending.take() {
                headers.entries.push(h);
            }
            match split_name_value(content) {
                Some(header) => pending = Some(header),
                None => {
                    // Doesn't look like a header and isn't a continuation of one:
                    // stop parsing headers here. This line and everything after
                    // it becomes the body.
                    return (headers, &raw[pos..]);
                }
            }
        }

        if nl.is_none() {
            // No more newlines: the remainder was the last header line, there is
            // no body.
            if let Some(h) = pending.take() {
                headers.entries.push(h);
            }
            pos = raw.len();
            break;
        }
        pos = next_pos;
    }

    (headers, raw.get(pos..).unwrap_or(&[]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_has_no_headers_and_empty_body() {
        let (headers, body) = split_headers(b"");
        assert!(headers.is_empty());
        assert!(body.is_empty());
    }

    #[test]
    fn no_colon_on_first_line_means_no_headers_at_all() {
        let raw = b"this is not a header line\r\nsecond line\r\n";
        let (headers, body) = split_headers(raw);
        assert!(headers.is_empty());
        assert_eq!(body, &raw[..]);
    }

    #[test]
    fn simple_headers_and_body_split_on_blank_line() {
        let raw = b"Subject: hi\r\nFrom: a@b.test\r\n\r\nbody text";
        let (headers, body) = split_headers(raw);
        assert_eq!(headers.get_first("Subject"), Some("hi"));
        assert_eq!(headers.get_first("From"), Some("a@b.test"));
        assert_eq!(body, b"body text");
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let (headers, _) = split_headers(b"sUbJeCt: x\r\n\r\n");
        assert_eq!(headers.get_first("SUBJECT"), Some("x"));
    }

    #[test]
    fn headers_debug_never_contains_a_header_value() {
        // T-093 review finding (round 3): `Headers` was still on
        // `#[derive(Debug)]`. Nothing in the tree currently prints a bare
        // `Headers` value -- `ParsedMessage::fmt` only ever calls
        // `headers.len()` -- but that was luck, not a guarantee: the exact
        // same situation `BodyContent` was in before this task started.
        // One marker per plausible leak: the value of a real header, and
        // (as a control) the header's own name, which is allowed to print.
        let value_canary = "SECRET-D14-HEADER-VALUE-MARKER";
        let raw = format!("Subject: {value_canary}\r\n\r\n");
        let (headers, _) = split_headers(raw.as_bytes());
        assert_eq!(
            headers.get_first("Subject"),
            Some(value_canary),
            "sanity check"
        );

        let debugged = format!("{headers:?}");
        assert!(
            !debugged.contains(value_canary),
            "Debug leaked a header value: {debugged}"
        );
        assert!(
            debugged.contains("Subject"),
            "header names are not content and may print: {debugged}"
        );
    }

    #[test]
    fn folded_header_value_is_unfolded() {
        let raw = b"Subject: hello\r\n  world\r\n\r\nbody";
        let (headers, body) = split_headers(raw);
        assert_eq!(headers.get_first("Subject"), Some("hello  world"));
        assert_eq!(body, b"body");
    }

    #[test]
    fn repeated_headers_are_all_kept() {
        let raw = b"Received: one\r\nReceived: two\r\n\r\n";
        let (headers, _) = split_headers(raw);
        let all: Vec<_> = headers.get_all("Received").collect();
        assert_eq!(all, vec!["one", "two"]);
    }

    #[test]
    fn header_without_colon_mid_stream_stops_header_parsing() {
        let raw = b"Subject: hi\r\nthis has no colon at all\r\nFrom: a@b\r\n\r\nreal body";
        let (headers, body) = split_headers(raw);
        assert_eq!(headers.get_first("Subject"), Some("hi"));
        // Everything from the malformed line onward is treated as body.
        assert!(body.starts_with(b"this has no colon"));
    }

    #[test]
    fn no_blank_line_separator_means_whole_message_is_headers() {
        let raw = b"Subject: hi\r\nFrom: a@b.test\r\n";
        let (headers, body) = split_headers(raw);
        assert_eq!(headers.get_first("Subject"), Some("hi"));
        assert!(body.is_empty());
    }

    #[test]
    fn only_blank_lines_body() {
        let raw = b"Subject: hi\r\n\r\n\r\n\r\n";
        let (_, body) = split_headers(raw);
        assert_eq!(body, b"\r\n\r\n");
    }
}
