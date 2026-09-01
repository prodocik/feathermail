//! `Content-Type` and `Content-Disposition` header value parsing:
//! `type/subtype; param="quoted value"; other=token`.

/// Splits `value` on top-level occurrences of `sep`, ignoring `sep` inside a
/// double-quoted string (with backslash-escaping honored inside quotes).
/// Operates purely on `char_indices`, so every slice boundary produced is a
/// valid UTF-8 boundary; terminates because the loop is bounded by the
/// number of characters in `value`.
fn split_top_level(value: &str, sep: char) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0usize;
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, c) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_quotes && c == '\\' {
            escaped = true;
            continue;
        }
        if c == '"' {
            in_quotes = !in_quotes;
            continue;
        }
        if c == sep && !in_quotes {
            result.push(&value[start..i]);
            start = i + c.len_utf8();
        }
    }
    result.push(&value[start..]);
    result
}

/// Removes a leading/trailing `"` and un-escapes `\x` -> `x` inside. Lenient:
/// an unterminated quoted string just takes the rest of the input.
fn unquote(s: &str) -> String {
    let mut chars = s.chars();
    if chars.next() != Some('"') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut escaped = false;
    for c in chars {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => break,
            _ => out.push(c),
        }
    }
    out
}

fn parse_param(raw: &str) -> Option<(String, String)> {
    let raw = raw.trim();
    let eq = raw.find('=')?;
    let name = raw[..eq].trim();
    if name.is_empty() {
        return None;
    }
    let val_part = raw[eq + 1..].trim();
    let value = if val_part.starts_with('"') {
        unquote(val_part)
    } else {
        val_part.to_string()
    };
    Some((name.to_ascii_lowercase(), value))
}

/// How a raw parameter name decomposes under RFC 2231: the base name, the
/// continuation index (`None` for a value that is not split), and whether
/// the segment's value is percent-encoded (the trailing `*`).
fn split_rfc2231_key(key: &str) -> (&str, Option<u32>, bool) {
    let (head, encoded) = match key.strip_suffix('*') {
        Some(head) => (head, true),
        None => (key, false),
    };
    match head.rsplit_once('*') {
        // `name*0`, `name*1*`, ... — a continuation segment. Only a decimal
        // index counts, so a parameter that genuinely contains a `*` is
        // left alone.
        Some((base, index)) if !base.is_empty() => match index.parse::<u32>() {
            Ok(index) => (base, Some(index), encoded),
            Err(_) => (key, None, false),
        },
        // `name*` — a single extended value; `name` — an ordinary one.
        _ if encoded => (head, None, true),
        _ => (key, None, false),
    }
}

/// Percent-decodes an RFC 2231 segment into bytes. A malformed escape
/// (`%ZZ`, a truncated `%4`) is kept literally rather than dropped: the
/// point here is a filename to show a reader, and refusing the whole
/// parameter over one bad byte is how attachments lose their names.
fn percent_decode(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// A decoded `filename`/`name` must name a file, not a place to put it.
/// Returns `None` for a value with no usable component left, so the caller
/// keeps no name at all rather than a name it had to invent.
fn safe_file_param(value: String) -> Option<String> {
    // A NUL is never part of a real filename and is exactly the trick that
    // makes one string look like two to a C API. Refuse the whole value.
    if value.contains('\0') {
        return None;
    }
    let base = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    Some(base)
}

/// One `name*N*=` segment of an RFC 2231 extended parameter value.
struct Rfc2231Segment {
    /// Continuation index; `0` for a value written in one piece.
    index: u32,
    /// The trailing `*`: this segment's value is percent-encoded.
    encoded: bool,
    value: String,
}

/// Every segment written for one base parameter name.
struct Rfc2231Group {
    base: String,
    segments: Vec<Rfc2231Segment>,
}

/// Joins and decodes RFC 2231 parameter values (`filename*=UTF-8''%D0%94…`,
/// `filename*0*=…; filename*1*=…`) into ordinary `name`/`value` pairs, so
/// the rest of the crate — and [`ContentType::param`] with it — never has
/// to know the encoding existed.
///
/// RFC 2231 §4: the extended form wins over a plain parameter of the same
/// name when a sender writes both (the plain one is the lossy fallback).
fn normalize_rfc2231(params: Vec<(String, String)>) -> Vec<(String, String)> {
    // Extended-form segments, grouped by base name in first-seen order.
    let mut extended: Vec<Rfc2231Group> = Vec::new();
    let mut plain: Vec<(String, String)> = Vec::new();

    for (key, value) in params {
        let (base, index, encoded) = split_rfc2231_key(&key);
        if (index.is_none() && !encoded) || base.is_empty() {
            plain.push((key, value));
            continue;
        }
        let base = base.to_string();
        let segment = Rfc2231Segment {
            index: index.unwrap_or(0),
            encoded,
            value,
        };
        match extended.iter_mut().find(|group| group.base == base) {
            Some(group) => group.segments.push(segment),
            None => extended.push(Rfc2231Group {
                base,
                segments: vec![segment],
            }),
        }
    }

    for Rfc2231Group { base, mut segments } in extended {
        segments.sort_by_key(|segment| segment.index);
        let mut charset = String::new();
        let mut bytes = Vec::new();
        for (position, segment) in segments.iter().enumerate() {
            let value = &segment.value;
            if !segment.encoded {
                bytes.extend_from_slice(value.as_bytes());
                continue;
            }
            if position == 0 {
                // `charset'language'value` — both fields may be empty
                // (`''%41.txt`), and only the first segment carries them.
                let mut fields = value.splitn(3, '\'');
                let declared = fields.next().unwrap_or_default();
                let has_prefix = fields.next().is_some();
                let rest = fields.next();
                if let (true, Some(rest)) = (has_prefix, rest) {
                    charset = declared.to_string();
                    bytes.extend_from_slice(&percent_decode(rest));
                    continue;
                }
            }
            bytes.extend_from_slice(&percent_decode(value));
        }

        let decoded = crate::charset::decode_bytes(&charset, &bytes);
        let decoded = if base.eq_ignore_ascii_case("filename") || base.eq_ignore_ascii_case("name")
        {
            match safe_file_param(decoded) {
                Some(name) => name,
                None => continue,
            }
        } else {
            decoded
        };

        match plain.iter_mut().find(|(name, _)| *name == base) {
            Some((_, existing)) => *existing = decoded,
            None => plain.push((base, decoded)),
        }
    }

    plain
}

/// Parsed `Content-Type` header: `type`, `subtype`, and parameters
/// (`charset`, `boundary`, `name`, ...).
#[derive(Debug, Clone)]
pub struct ContentType {
    type_: String,
    subtype: String,
    params: Vec<(String, String)>,
}

impl Default for ContentType {
    /// RFC 2045 §5.2 default when `Content-Type` is absent: `text/plain`.
    fn default() -> Self {
        ContentType {
            type_: "text".to_string(),
            subtype: "plain".to_string(),
            params: Vec::new(),
        }
    }
}

impl ContentType {
    /// Parses a `Content-Type` header value. Never fails: a value that
    /// doesn't look like `type/subtype` falls back to the RFC 2045 default
    /// (`text/plain`), same as a missing header.
    pub fn parse(value: &str) -> Self {
        let segments = split_top_level(value, ';');
        let mut iter = segments.into_iter();
        let head = iter.next().unwrap_or("").trim();
        let (type_, subtype) = match head.split_once('/') {
            Some((t, s)) if !t.trim().is_empty() && !s.trim().is_empty() => {
                (t.trim().to_ascii_lowercase(), s.trim().to_ascii_lowercase())
            }
            _ => ("text".to_string(), "plain".to_string()),
        };
        let params = normalize_rfc2231(iter.filter_map(parse_param).collect());
        ContentType {
            type_,
            subtype,
            params,
        }
    }

    pub fn type_(&self) -> &str {
        &self.type_
    }

    pub fn subtype(&self) -> &str {
        &self.subtype
    }

    /// `"type/subtype"`, both lowercased.
    pub fn full(&self) -> String {
        format!("{}/{}", self.type_, self.subtype)
    }

    pub fn is(&self, type_: &str, subtype: &str) -> bool {
        self.type_.eq_ignore_ascii_case(type_) && self.subtype.eq_ignore_ascii_case(subtype)
    }

    pub fn is_multipart(&self) -> bool {
        self.type_.eq_ignore_ascii_case("multipart")
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn charset(&self) -> Option<&str> {
        self.param("charset")
    }

    pub fn boundary(&self) -> Option<&str> {
        self.param("boundary")
    }

    pub fn name(&self) -> Option<&str> {
        self.param("name")
    }
}

/// Parsed `Content-Disposition` header: the disposition keyword
/// (`inline`/`attachment`/...) and its parameters (`filename`, ...).
#[derive(Debug, Clone)]
pub struct ContentDisposition {
    disposition: String,
    params: Vec<(String, String)>,
}

impl ContentDisposition {
    pub fn parse(value: &str) -> Self {
        let segments = split_top_level(value, ';');
        let mut iter = segments.into_iter();
        let disposition = iter.next().unwrap_or("").trim().to_ascii_lowercase();
        let params = normalize_rfc2231(iter.filter_map(parse_param).collect());
        ContentDisposition {
            disposition,
            params,
        }
    }

    pub fn disposition(&self) -> &str {
        &self.disposition
    }

    pub fn is_attachment(&self) -> bool {
        self.disposition.eq_ignore_ascii_case("attachment")
    }

    pub fn filename(&self) -> Option<&str> {
        self.params
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("filename"))
            .map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_type() {
        let ct = ContentType::parse("text/html");
        assert!(ct.is("text", "html"));
        assert_eq!(ct.charset(), None);
    }

    #[test]
    fn params_with_quotes_and_escapes() {
        let ct = ContentType::parse(r#"multipart/mixed; boundary="a\"b--c"; charset=UTF-8"#);
        assert!(ct.is("multipart", "mixed"));
        assert_eq!(ct.boundary(), Some(r#"a"b--c"#));
        assert_eq!(ct.charset(), Some("UTF-8"));
    }

    #[test]
    fn semicolon_inside_quoted_value_is_not_a_separator() {
        let ct = ContentType::parse(r#"text/plain; name="a;b.txt""#);
        assert_eq!(ct.name(), Some("a;b.txt"));
    }

    #[test]
    fn malformed_type_falls_back_to_default() {
        let ct = ContentType::parse("garbage-no-slash");
        assert!(ct.is("text", "plain"));
    }

    #[test]
    fn empty_value_falls_back_to_default() {
        let ct = ContentType::parse("");
        assert!(ct.is("text", "plain"));
    }

    #[test]
    fn param_lookup_is_case_insensitive() {
        let ct = ContentType::parse("text/plain; CHARSET=koi8-r");
        assert_eq!(ct.charset(), Some("koi8-r"));
    }

    #[test]
    fn content_disposition_attachment_with_filename() {
        let cd = ContentDisposition::parse(r#"attachment; filename="report.pdf""#);
        assert!(cd.is_attachment());
        assert_eq!(cd.filename(), Some("report.pdf"));
    }

    #[test]
    fn content_disposition_inline_is_not_attachment() {
        let cd = ContentDisposition::parse("inline");
        assert!(!cd.is_attachment());
    }

    #[test]
    fn unterminated_quote_does_not_panic() {
        let ct = ContentType::parse(r#"text/plain; name="unterminated"#);
        assert_eq!(ct.name(), Some("unterminated"));
    }

    #[test]
    fn rfc2231_ext_value_filename_is_decoded() {
        let cd = ContentDisposition::parse(
            "attachment; filename*=UTF-8''%D0%94%D0%BE%D0%B3%D0%BE%D0%B2%D0%BE%D1%80.pdf",
        );
        assert_eq!(cd.filename(), Some("Договор.pdf"));
    }

    #[test]
    fn rfc2231_continuations_are_joined_in_numeric_order() {
        // Ten-plus segments are the reason the index is compared as a
        // number: sorted as text, `filename*10*` would land before
        // `filename*2*` and quietly scramble the name.
        let cd = ContentDisposition::parse(
            "attachment; filename*0*=UTF-8''%D0%94%D0%BE; filename*1*=%D0%B3%D0%BE%D0%B2%D0%BE%D1%80.pdf",
        );
        assert_eq!(cd.filename(), Some("Договор.pdf"));

        let mut header = String::from("attachment");
        for (index, chunk) in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"]
            .iter()
            .enumerate()
        {
            header.push_str(&format!("; filename*{index}={chunk}"));
        }
        assert_eq!(
            ContentDisposition::parse(&header).filename(),
            Some("abcdefghijk")
        );
    }

    #[test]
    fn rfc2231_name_on_content_type_is_decoded_too() {
        let ct = ContentType::parse("application/pdf; name*=UTF-8''%D0%94%D0%BE%D0%B3.pdf");
        assert_eq!(ct.name(), Some("Дог.pdf"));
    }

    #[test]
    fn plain_filename_still_works() {
        let cd = ContentDisposition::parse("attachment; filename=\"report.pdf\"");
        assert_eq!(cd.filename(), Some("report.pdf"));
    }

    #[test]
    fn rfc2047_encoded_word_filename_is_left_for_the_word_decoder() {
        // RFC 2047 in a parameter is not RFC 2231 and is not this module's
        // job: it has to survive untouched for `rfc2047::decode_encoded_words`
        // in `select.rs`.
        let cd = ContentDisposition::parse("attachment; filename=\"=?utf-8?B?0JTQvtCzLnBkZg==?=\"");
        assert_eq!(cd.filename(), Some("=?utf-8?B?0JTQvtCzLnBkZg==?="));
    }

    #[test]
    fn rfc2231_with_an_empty_charset_falls_back_to_utf8() {
        let cd = ContentDisposition::parse("attachment; filename*=''%41.txt");
        assert_eq!(cd.filename(), Some("A.txt"));
    }

    #[test]
    fn rfc2231_in_a_legacy_charset_is_decoded_with_it() {
        // windows-1251 %CF%F0%E8 = "При"
        let cd = ContentDisposition::parse("attachment; filename*=windows-1251''%CF%F0%E8.txt");
        assert_eq!(cd.filename(), Some("При.txt"));
    }

    #[test]
    fn rfc2231_with_a_broken_percent_escape_does_not_panic() {
        let cd = ContentDisposition::parse("attachment; filename*=UTF-8''a%ZZb%4.txt");
        assert_eq!(cd.filename(), Some("a%ZZb%4.txt"));
    }

    #[test]
    fn rfc2231_extended_form_wins_over_the_plain_parameter() {
        // RFC 2231 §4: the plain parameter is the lossy fallback a sender
        // writes for old clients; the extended one is the real name.
        let cd = ContentDisposition::parse(
            "attachment; filename=\"fallback.pdf\"; filename*=UTF-8''%D0%94%D0%BE%D0%B3.pdf",
        );
        assert_eq!(cd.filename(), Some("Дог.pdf"));
    }

    #[test]
    fn a_decoded_filename_can_never_carry_a_path() {
        let cd = ContentDisposition::parse("attachment; filename*=UTF-8''..%2F..%2Fetc%2Fpasswd");
        assert_eq!(cd.filename(), Some("passwd"));

        let cd = ContentDisposition::parse("attachment; filename*=UTF-8''C%3A%5Cwin%5Cevil.exe");
        assert_eq!(cd.filename(), Some("evil.exe"));

        // A NUL is never a filename: keep no name rather than half of one.
        let cd = ContentDisposition::parse("attachment; filename*=UTF-8''safe.txt%00.exe");
        assert_eq!(cd.filename(), None);

        let cd = ContentDisposition::parse("attachment; filename*=UTF-8''%2E%2E");
        assert_eq!(cd.filename(), None);
    }

    #[test]
    fn a_star_in_an_ordinary_parameter_name_is_not_a_continuation() {
        let ct = ContentType::parse("text/plain; odd*name=value; charset=utf-8");
        assert_eq!(ct.param("odd*name"), Some("value"));
        assert_eq!(ct.charset(), Some("utf-8"));
    }
}
