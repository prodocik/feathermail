//! Charset decoding to UTF-8.
//!
//! Supported: `utf-8`, `us-ascii`, `iso-8859-1`, `windows-1251`, `koi8-r`
//! (plus common aliases). Everything else falls back to lossy UTF-8 — see
//! the rationale on `decode_bytes` below. This module never panics: every
//! table lookup is bounds-checked by construction (byte - 0x80 is always
//! 0..=127) and `String::from_utf8_lossy` never fails.

/// windows-1251 (CP1251), bytes 0x80..=0xFF -> Unicode codepoint.
/// 0xC0..=0xFF is the contiguous Cyrillic alphabet block (this is the
/// defining, well-known structural property of cp1251); 0x80..=0xBF is
/// Latin-1-style punctuation/typography plus a handful of extra Cyrillic
/// letters (Ѓ Љ Њ Ќ Ћ Џ / ѓ љ њ ќ ћ џ) and Ukrainian/Belarusian letters.
/// Slot 0x98 is unassigned in cp1251 and maps to the replacement character.
const CP1251_HIGH: [char; 128] = [
    '\u{0402}', '\u{0403}', '\u{201A}', '\u{0453}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{20AC}', '\u{2030}', '\u{0409}', '\u{2039}', '\u{040A}', '\u{040C}', '\u{040B}', '\u{040F}',
    '\u{0452}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{FFFD}', '\u{2122}', '\u{0459}', '\u{203A}', '\u{045A}', '\u{045C}', '\u{045B}', '\u{045F}',
    '\u{00A0}', '\u{040E}', '\u{045E}', '\u{0408}', '\u{00A4}', '\u{0490}', '\u{00A6}', '\u{00A7}',
    '\u{0401}', '\u{00A9}', '\u{0404}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{0407}',
    '\u{00B0}', '\u{00B1}', '\u{0406}', '\u{0456}', '\u{0491}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{0451}', '\u{2116}', '\u{0454}', '\u{00BB}', '\u{0458}', '\u{0405}', '\u{0455}', '\u{0457}',
    '\u{0410}', '\u{0411}', '\u{0412}', '\u{0413}', '\u{0414}', '\u{0415}', '\u{0416}', '\u{0417}',
    '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}', '\u{041D}', '\u{041E}', '\u{041F}',
    '\u{0420}', '\u{0421}', '\u{0422}', '\u{0423}', '\u{0424}', '\u{0425}', '\u{0426}', '\u{0427}',
    '\u{0428}', '\u{0429}', '\u{042A}', '\u{042B}', '\u{042C}', '\u{042D}', '\u{042E}', '\u{042F}',
    '\u{0430}', '\u{0431}', '\u{0432}', '\u{0433}', '\u{0434}', '\u{0435}', '\u{0436}', '\u{0437}',
    '\u{0438}', '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}', '\u{043D}', '\u{043E}', '\u{043F}',
    '\u{0440}', '\u{0441}', '\u{0442}', '\u{0443}', '\u{0444}', '\u{0445}', '\u{0446}', '\u{0447}',
    '\u{0448}', '\u{0449}', '\u{044A}', '\u{044B}', '\u{044C}', '\u{044D}', '\u{044E}', '\u{044F}',
];

/// KOI8-R, bytes 0x80..=0xFF -> Unicode codepoint.
///
/// 0xC0..=0xFF carries the actual Cyrillic letters (lowercase then
/// uppercase, in KOI8's characteristic Latin-lookalike order) plus ё/Ё at
/// 0xA3/0xB3 — this is the part that matters for real Russian prose and is
/// mapped precisely. 0x80..=0xBF (minus 0xA3/0xB3) is legacy box-drawing and
/// math pseudographics from the original KOI8 charset; those code points are
/// not reproduced here (risk of a subtly wrong table for characters that
/// essentially never appear in mail bodies) and decode to the replacement
/// character instead of guessing.
const KOI8R_HIGH: [char; 128] = {
    let mut table = ['\u{FFFD}'; 128];
    table[0x23] = '\u{0451}'; // 0xA3 = ё
    table[0x33] = '\u{0401}'; // 0xB3 = Ё
                              // 0xC0..=0xDF: ю а б ц д е ф г х и й к л м н о
    let lower = [
        '\u{044E}', '\u{0430}', '\u{0431}', '\u{0446}', '\u{0434}', '\u{0435}', '\u{0444}',
        '\u{0433}', '\u{0445}', '\u{0438}', '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}',
        '\u{043D}', '\u{043E}', '\u{043F}', '\u{044F}', '\u{0440}', '\u{0441}', '\u{0442}',
        '\u{0443}', '\u{0436}', '\u{0432}', '\u{044C}', '\u{044B}', '\u{0437}', '\u{0448}',
        '\u{044D}', '\u{0449}', '\u{0447}', '\u{044A}',
    ];
    // 0xE0..=0xFF: uppercase counterparts, same order
    let upper = [
        '\u{042E}', '\u{0410}', '\u{0411}', '\u{0426}', '\u{0414}', '\u{0415}', '\u{0424}',
        '\u{0413}', '\u{0425}', '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}',
        '\u{041D}', '\u{041E}', '\u{041F}', '\u{042F}', '\u{0420}', '\u{0421}', '\u{0422}',
        '\u{0423}', '\u{0416}', '\u{0412}', '\u{042C}', '\u{042B}', '\u{0417}', '\u{0428}',
        '\u{042D}', '\u{0429}', '\u{0427}', '\u{042A}',
    ];
    let mut i = 0;
    while i < 32 {
        table[0x40 + i] = lower[i]; // 0xC0..=0xDF
        table[0x60 + i] = upper[i]; // 0xE0..=0xFF
        i += 1;
    }
    table
};

fn normalize_charset_name(charset: &str) -> String {
    charset.trim().trim_matches('"').to_ascii_lowercase()
}

fn decode_single_byte(bytes: &[u8], high: &[char; 128]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b < 0x80 {
                b as char
            } else {
                high[(b - 0x80) as usize]
            }
        })
        .collect()
}

fn decode_latin1(bytes: &[u8]) -> String {
    // ISO-8859-1 maps byte N to Unicode codepoint N for the whole range, always.
    bytes.iter().map(|&b| b as char).collect()
}

fn decode_ascii(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' })
        .collect()
}

/// Decodes `bytes` as `charset` into a `String`. Never panics and never
/// returns an error: an unrecognized charset name falls back to lossy UTF-8
/// decoding rather than refusing to show the part at all.
///
/// Rationale for the fallback (deliberately chosen, not an oversight):
/// mail in the wild mislabels charsets constantly, and most "unknown" labels
/// this crate will actually see in practice are either already UTF-8 (or an
/// ASCII-compatible subset of it), or a charset genuinely not worth a table
/// for out of five languages the user reads. Lossy UTF-8 degrades to the
/// *correct* output whenever the bytes are plain ASCII (by far the common
/// case for headers/params carrying a bogus charset name), and only shows
/// U+FFFD replacement characters for the byte sequences that are truly
/// undecodable — which is strictly more useful than an empty body, and can
/// never panic. It is not "correct" for genuinely-unlisted single-byte
/// legacy encodings (e.g. iso-8859-5); that is the accepted gap here.
pub fn decode_bytes(charset: &str, bytes: &[u8]) -> String {
    match normalize_charset_name(charset).as_str() {
        "utf-8" | "utf8" => String::from_utf8_lossy(bytes).into_owned(),
        "us-ascii" | "ascii" | "ansi_x3.4-1968" => decode_ascii(bytes),
        "iso-8859-1" | "iso8859-1" | "latin1" | "l1" => decode_latin1(bytes),
        "windows-1251" | "cp1251" | "win-1251" | "win1251" => {
            decode_single_byte(bytes, &CP1251_HIGH)
        }
        "koi8-r" | "koi8r" => decode_single_byte(bytes, &KOI8R_HIGH),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_valid_passthrough() {
        assert_eq!(decode_bytes("utf-8", "привет".as_bytes()), "привет");
    }

    #[test]
    fn utf8_invalid_bytes_do_not_panic() {
        // Claims utf-8 but is not valid utf-8: must not panic, must produce
        // *some* string (with replacement characters).
        let bytes = [0xFF, 0xFE, b'h', b'i'];
        let out = decode_bytes("utf-8", &bytes);
        assert!(out.ends_with("hi"));
    }

    #[test]
    fn windows_1251_decodes_privet() {
        // "привет" in cp1251.
        let bytes = [0xEF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
        assert_eq!(decode_bytes("windows-1251", &bytes), "привет");
    }

    #[test]
    fn koi8_r_decodes_privet() {
        // "привет" in koi8-r.
        let bytes = [0xD0, 0xD2, 0xC9, 0xD7, 0xC5, 0xD4];
        assert_eq!(decode_bytes("koi8-r", &bytes), "привет");
    }

    #[test]
    fn koi8_r_decodes_yo() {
        assert_eq!(decode_bytes("koi8-r", &[0xA3]), "ё");
        assert_eq!(decode_bytes("koi8-r", &[0xB3]), "Ё");
    }

    #[test]
    fn iso_8859_1_is_identity_on_codepoints() {
        assert_eq!(decode_bytes("iso-8859-1", &[0xE9]), "\u{00E9}"); // é
    }

    #[test]
    fn unknown_charset_falls_back_to_lossy_utf8_ascii_subset() {
        assert_eq!(decode_bytes("x-made-up-charset", b"hello"), "hello");
    }

    #[test]
    fn charset_name_matching_is_case_and_quote_insensitive() {
        let bytes = [0xEF, 0xF0];
        assert_eq!(
            decode_bytes("\"WINDOWS-1251\"", &bytes),
            decode_bytes("windows-1251", &bytes)
        );
    }
}
