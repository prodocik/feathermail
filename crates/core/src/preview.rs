//! Safe, bounded message previews used by Inbox rows.
//!
//! The helper accepts raw RFC 822 bytes but never exposes them through a
//! debug/error path. Plain text is normalized directly; HTML is sanitized by
//! the existing HTML security boundary before tags are reduced to text.

use feathermail_html::{parse_message, sanitize, BodyContent, SanitizeOptions};

/// Maximum number of Unicode scalar values retained in a stored preview.
/// Keeping the bound in characters (rather than bytes) prevents UTF-8 from
/// being split while keeping row rendering bounded for hostile mail.
pub const DEFAULT_PREVIEW_CHARS: usize = 240;

/// Build a display preview from raw RFC 822/MIME bytes.
///
/// Empty bodies and undecodable MIME parts intentionally produce an empty
/// string. This function is pure and does no I/O, logging, or database work.
pub fn preview_from_raw_mime(raw: &[u8], max_chars: usize) -> String {
    if max_chars == 0 || raw.is_empty() {
        return String::new();
    }
    let parsed = parse_message(raw, true);
    let text = match parsed.body {
        BodyContent::Plain(text) => text,
        BodyContent::Html(html) => {
            let (sanitized, _) = sanitize(&html, &SanitizeOptions::default());
            html_to_text(sanitized.as_sanitized_str())
        }
        BodyContent::Empty | BodyContent::Undecodable(_) => return String::new(),
    };
    bound_preview(&normalize_whitespace(&text), max_chars)
}

fn normalize_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn bound_preview(text: &str, max_chars: usize) -> String {
    let mut out = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Reduce sanitized HTML to readable text. The sanitizer has already
/// removed scripts, event handlers, and remote resources; this pass is only
/// presentation extraction and deliberately has no HTML interpretation side
/// effects. Common entities are decoded so previews do not show markup.
fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut entity = String::new();
    for ch in html.chars() {
        // T-134: inside a tag nothing is text -- and nothing is an entity
        // either. `<img src="...?a=1&b=2&c=3">` used to feed every `&` of
        // a tracking URL into the entity buffer, which then spilled into
        // the preview as `&&&&&&&&` and swallowed the `>` that ends the
        // tag. Owner, on the live list: «в карточке письма в некоторых
        // письмах где есть картинки пишет &&&&&&&& за тем текст письма».
        if in_tag {
            if ch == '>' {
                in_tag = false;
                text.push(' ');
            }
            continue;
        }
        if !entity.is_empty() {
            if ch == ';' {
                text.push_str(&decode_entity(&entity));
                entity.clear();
            } else if entity.len() < 16 && (ch.is_ascii_alphanumeric() || ch == '#') {
                entity.push(ch);
            } else {
                // Not an entity after all: hand back what was collected,
                // then let this character take its normal path.
                text.push_str(&entity);
                entity.clear();
                if ch == '<' {
                    in_tag = true;
                    text.push(' ');
                } else if ch == '&' {
                    entity.push('&');
                } else {
                    text.push(ch);
                }
            }
            continue;
        }
        if ch == '&' {
            entity.push('&');
        } else if ch == '<' {
            in_tag = true;
            text.push(' ');
        } else {
            text.push(ch);
        }
    }
    if !entity.is_empty() {
        text.push_str(&entity);
    }
    text
}

/// T-134: `&name` / `&#39` / `&#x27` -> the character it stands for, or
/// the text as written when it stands for nothing this cares about. Takes
/// the entity without its trailing `;` (the caller has already eaten it).
fn decode_entity(entity: &str) -> String {
    match entity {
        "&nbsp" => return " ".to_string(),
        "&amp" => return "&".to_string(),
        "&lt" => return "<".to_string(),
        "&gt" => return ">".to_string(),
        "&quot" => return "\"".to_string(),
        "&apos" | "&#39" | "&#x27" | "&#X27" => return "'".to_string(),
        _ => {}
    }
    // Numeric entities: a letter body full of `&#1055;&#1088;...` is one
    // Cyrillic word, not eight escapes across a card.
    if let Some(digits) = entity.strip_prefix("&#") {
        let code = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => digits.parse::<u32>().ok(),
        };
        if let Some(ch) = code.and_then(char::from_u32) {
            return ch.to_string();
        }
    }
    entity.to_string()
}

#[cfg(test)]
mod tests {

    /// T-134. Owner, on the live list: «в карточке письма в некоторых
    /// письмах где есть картинки пишет &&&&&&&& за тем текст письма».
    /// A tracking image's query string is a run of `&`, and every one of
    /// them used to start an entity that never closed -- the buffer spilled
    /// into the card and ate the `>` that ends the tag with it.
    #[test]
    fn a_tracking_image_url_never_leaks_ampersands_into_the_card() {
        let raw = concat!(
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "<p><img src=\"https://t.example.com/o?a=1&b=2&c=3&d=4&e=5\" width=\"1\">",
            "Настоящий текст письма</p>"
        )
        .as_bytes();
        let preview = preview_from_raw_mime(raw, 200);
        assert!(
            !preview.contains('&'),
            "no ampersand from inside a tag may reach the card, got {preview:?}"
        );
        assert!(
            preview.contains("Настоящий текст письма"),
            "and the letter's own text still arrives, got {preview:?}"
        );
    }

    /// The `>` that closes such a tag has to close it: without this the
    /// rest of the letter was read as if it were still inside the tag.
    #[test]
    fn a_tag_that_holds_an_ampersand_still_ends() {
        assert_eq!(
            super::html_to_text("<a href=\"x?a=1&b=2\">text</a>after")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            "text after"
        );
    }

    /// Numeric entities are what a Cyrillic subject line often arrives as.
    #[test]
    fn numeric_entities_decode_to_their_characters() {
        assert_eq!(super::decode_entity("&#1055"), "П");
        assert_eq!(super::decode_entity("&#x41"), "A");
        assert_eq!(super::decode_entity("&#39"), "'");
        assert_eq!(
            super::decode_entity("&nonsense"),
            "&nonsense",
            "an entity this does not know stays exactly as written"
        );
    }
    use super::*;

    #[test]
    fn plain_mime_preview_normalizes_and_bounds_utf8_safely() {
        let raw = b"Content-Type: text/plain; charset=utf-8\r\n\r\n  hello\r\n\r\n world  ";
        assert_eq!(preview_from_raw_mime(raw, 32), "hello world");
        assert_eq!(preview_from_raw_mime("Привет мир".as_bytes(), 6), "Привет…");
    }

    #[test]
    fn sanitized_html_preview_drops_markup_and_decodes_entities() {
        let raw = b"Content-Type: text/html; charset=utf-8\r\n\r\n<p>Hello&nbsp;<strong>world</strong></p><script>secret()</script>";
        assert_eq!(preview_from_raw_mime(raw, 240), "Hello world");
    }

    #[test]
    fn empty_or_undecodable_preview_is_empty() {
        assert_eq!(preview_from_raw_mime(b"", 240), "");
        let raw = b"Content-Type: text/plain\r\nContent-Transfer-Encoding: x-unknown\r\n\r\nbody";
        assert_eq!(preview_from_raw_mime(raw, 240), "");
    }

    /// T-120: the inbox card reads the same sanitizer as the letter, so a
    /// `font-size:0` preheader must not become the two-line preview.
    #[test]
    fn html_preheader_is_not_the_inbox_preview() {
        let raw = concat!(
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            r#"<span style="font-size:0px;line-height:0px">PREHEADER_TOKEN</span>"#,
            "<p>Hello world from the body</p>",
        );
        let preview = preview_from_raw_mime(raw.as_bytes(), 240);
        assert!(!preview.contains("PREHEADER_TOKEN"), "{preview}");
        assert!(preview.contains("Hello world from the body"), "{preview}");
    }
}
