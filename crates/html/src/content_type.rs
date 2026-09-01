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
        let params = iter.filter_map(parse_param).collect();
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
        let params = iter.filter_map(parse_param).collect();
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
}
