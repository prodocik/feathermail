//! RFC 2047 encoded-word decoding for header values: `=?charset?B?...?=` and
//! `=?charset?Q?...?=`, e.g. in `Subject:` / `From:` display names.

use crate::charset::decode_bytes;
use crate::transfer::decode_base64;

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// RFC 2047 "Q" encoding: like quoted-printable, but `_` decodes to a space.
fn decode_q(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' => {
                if let (Some(&h1), Some(&h2)) = (input.get(i + 1), input.get(i + 2)) {
                    if let (Some(hi), Some(lo)) = (hex_val(h1), hex_val(h2)) {
                        out.push((hi << 4) | lo);
                        i += 3;
                        continue;
                    }
                }
                // Truncated/invalid escape: drop the lone '=' and continue.
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Tries to decode a single encoded word starting at the beginning of `s`
/// (`s` is not required to start with `=?`; returns `None` if it doesn't
/// parse as one). On success, returns the decoded text and how many bytes
/// of `s` the token consumed.
fn try_decode_token(s: &str) -> Option<(String, usize)> {
    if !s.starts_with("=?") {
        return None;
    }
    let after = &s[2..];
    let q1 = after.find('?')?;
    let charset = &after[..q1];
    if charset.is_empty() {
        return None;
    }
    let after2 = &after[q1 + 1..];
    let q2 = after2.find('?')?;
    let enc = &after2[..q2];
    let mut enc_chars = enc.chars();
    let enc_char = enc_chars.next()?;
    if enc_chars.next().is_some() {
        return None; // encoding letter must be exactly one char
    }
    let enc_char = enc_char.to_ascii_uppercase();
    if enc_char != 'B' && enc_char != 'Q' {
        return None;
    }
    let after3 = &after2[q2 + 1..];
    let end = after3.find("?=")?;
    let text = &after3[..end];

    let raw_bytes = match enc_char {
        'B' => decode_base64(text.as_bytes()),
        'Q' => decode_q(text.as_bytes()),
        _ => unreachable!("checked above"),
    };
    let decoded = decode_bytes(charset, &raw_bytes);
    let total_len = 2 + q1 + 1 + q2 + 1 + end + 2;
    Some((decoded, total_len))
}

/// Decodes all RFC 2047 encoded words in `input`, leaving any other text
/// untouched. Whitespace that separates two adjacent encoded words is
/// dropped (RFC 2047 §6.2), so `"=?utf-8?Q?Hello?= =?utf-8?Q?World?="`
/// decodes to `"HelloWorld"`, matching how mail clients join a long header
/// value that got split into several encoded words.
///
/// Never panics and never loops forever: `cursor` strictly increases every
/// iteration (by at least 2, the length of a literal `"=?"` that didn't
/// parse as a real token), bounded by `input.len()`.
pub fn decode_encoded_words(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut cursor = 0usize;
    let mut prev_was_encoded = false;

    while let Some(rest) = input.get(cursor..) {
        let Some(rel_start) = rest.find("=?") else {
            result.push_str(rest);
            break;
        };
        let start = cursor + rel_start;
        let between = &input[cursor..start];

        match try_decode_token(&input[start..]) {
            Some((decoded, consumed)) => {
                let between_is_only_ws =
                    !between.is_empty() && between.chars().all(char::is_whitespace);
                if !(prev_was_encoded && between_is_only_ws) {
                    result.push_str(between);
                }
                result.push_str(&decoded);
                cursor = start + consumed;
                prev_was_encoded = true;
            }
            None => {
                // Not actually a valid encoded word (e.g. a literal "=?" in
                // free text): copy it through and move past it so we always
                // make forward progress.
                result.push_str(between);
                result.push_str("=?");
                cursor = start + 2;
                prev_was_encoded = false;
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(decode_encoded_words("hello world"), "hello world");
    }

    #[test]
    fn decodes_utf8_base64_word() {
        // "привет" in UTF-8, base64.
        assert_eq!(
            decode_encoded_words("=?utf-8?B?0L/RgNC40LLQtdGC?="),
            "привет"
        );
    }

    #[test]
    fn decodes_koi8_r_quoted_printable_word() {
        // "тест" in koi8-r, Q-encoded: т=D4 е=C5 с=D3 т=D4
        assert_eq!(decode_encoded_words("=?koi8-r?Q?=D4=C5=D3=D4?="), "тест");
    }

    #[test]
    fn q_encoding_underscore_is_space() {
        assert_eq!(
            decode_encoded_words("=?utf-8?Q?hello_world?="),
            "hello world"
        );
    }

    #[test]
    fn adjacent_encoded_words_join_without_the_separating_whitespace() {
        let input = "=?utf-8?Q?Hello?= =?utf-8?Q?World?=";
        assert_eq!(decode_encoded_words(input), "HelloWorld");
    }

    #[test]
    fn non_adjacent_words_keep_surrounding_text() {
        let input = "prefix =?utf-8?Q?mid?= suffix";
        assert_eq!(decode_encoded_words(input), "prefix mid suffix");
    }

    #[test]
    fn malformed_token_does_not_panic_or_loop() {
        for s in [
            "=?",
            "=?utf-8",
            "=?utf-8?B",
            "=?utf-8?B?",
            "=?utf-8?B?abc",
            "=?=?=?=?",
        ] {
            let _ = decode_encoded_words(s);
        }
    }

    #[test]
    fn empty_input() {
        assert_eq!(decode_encoded_words(""), "");
    }
}
