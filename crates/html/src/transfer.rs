//! `Content-Transfer-Encoding` decoding: base64 and quoted-printable.
//!
//! Both decoders are best-effort and infallible (`Vec<u8>`, no `Result`):
//! they skip whatever they cannot make sense of instead of failing, because
//! real-world mail routinely has line-wrapped, whitespace-padded, or
//! truncated encoded bodies and a parser that refuses to show *anything*
//! because of one bad byte is worse than one that shows most of the message.

/// Decodes base64 (RFC 4648, without url-safe variant). Non-alphabet bytes
/// (newlines, stray whitespace, garbage) are skipped rather than rejected.
/// A trailing group with fewer than 2 usable characters is simply dropped
/// (there aren't enough bits to make a byte), so a truncated/corrupt tail
/// never panics and never blocks decoding of everything before it.
pub fn decode_base64(input: &[u8]) -> Vec<u8> {
    let mut table = [0xFFu8; 256];
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for (i, &b) in ALPHABET.iter().enumerate() {
        table[b as usize] = i as u8;
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &b in input {
        if b == b'=' {
            continue; // padding: ignored, end-of-data is handled by leftover bits below
        }
        let v = table[b as usize];
        if v == 0xFF {
            continue; // whitespace or garbage: skip
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodes quoted-printable (RFC 2045 §6.7): `=XX` hex escapes and soft line
/// breaks (`=` immediately before a line ending, which is removed along with
/// the line ending). All access is via `get()`/bounds checks, so a body that
/// ends mid-escape (`...=`, `...=A`) never indexes out of range: the
/// dangling `=` (and whatever partial hex digit follows it) is dropped.
pub fn decode_quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        let b = input[i];
        if b != b'=' {
            out.push(b);
            i += 1;
            continue;
        }

        if input.get(i + 1) == Some(&b'\r') && input.get(i + 2) == Some(&b'\n') {
            i += 3; // soft line break, CRLF form
            continue;
        }
        if input.get(i + 1) == Some(&b'\n') {
            i += 2; // soft line break, bare LF form
            continue;
        }
        if let (Some(&h1), Some(&h2)) = (input.get(i + 1), input.get(i + 2)) {
            if let (Some(hi), Some(lo)) = (hex_val(h1), hex_val(h2)) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        // A lone/invalid '=' (including one truncated at end of input): drop it
        // and keep going rather than failing the whole decode.
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip_simple() {
        assert_eq!(decode_base64(b"aGVsbG8="), b"hello");
    }

    #[test]
    fn base64_tolerates_line_wrapping_and_garbage() {
        assert_eq!(decode_base64(b"aGVs\r\nbG8=\n***"), b"hello");
    }

    #[test]
    fn base64_truncated_tail_does_not_panic() {
        // "aGVsbG8" without padding, plus a dangling partial group.
        let out = decode_base64(b"aGVsbG8=a");
        assert_eq!(&out[..5], b"hello");
    }

    #[test]
    fn base64_empty_input() {
        assert_eq!(decode_base64(b""), Vec::<u8>::new());
    }

    #[test]
    fn quoted_printable_hex_escape() {
        assert_eq!(decode_quoted_printable(b"caf=C3=A9"), b"caf\xC3\xA9");
    }

    #[test]
    fn quoted_printable_soft_line_break() {
        assert_eq!(
            decode_quoted_printable(b"long line=\r\ncontinues"),
            b"long linecontinues"
        );
    }

    #[test]
    fn quoted_printable_truncated_escape_does_not_panic() {
        // A dangling '=' at the very end (nothing follows it) is dropped.
        assert_eq!(decode_quoted_printable(b"abc="), b"abc");
        // '=A' with nothing after (no second hex digit) is not a valid
        // escape: the lone '=' is dropped, 'A' is kept as a literal char.
        assert_eq!(decode_quoted_printable(b"abc=A"), b"abcA");
        // '=A' followed by a non-hex char ('z') is likewise not a valid
        // escape: the lone '=' is dropped and 'A'/'z' are kept literally.
        assert_eq!(decode_quoted_printable(b"abc=Az"), b"abcAz");
    }
}
