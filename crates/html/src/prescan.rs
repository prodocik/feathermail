//! A read-only pass over the raw HTML that decides which `<img src>` values
//! must be blocked *unconditionally* (tracking pixels, T-030 / D44) and
//! counts what the sanitizer is about to remove, for [`crate::SanitizeReport`].
//!
//! This is not the security boundary — `sanitize.rs`'s allow-list is. This
//! module exists because `ammonia`'s `attribute_filter` callback sees one
//! attribute at a time and cannot correlate an `<img>`'s `src` with its
//! sibling `width`/`height` attributes. So we tokenize the input once,
//! ourselves, with the same HTML5 tokenizer `ammonia` uses internally
//! (`html5ever`) — not a hand-rolled regex — specifically so this pass
//! agrees with the sanitizer's own parsing of tag/attribute boundaries.
//! Disagreement here would only ever *under*-block (fail to add a src to
//! the force-block set), never open a hole: the base allow-list in
//! `sanitize.rs` still applies independently of anything computed here.

use std::cell::RefCell;
use std::collections::HashSet;

use html5ever::tendril::ByteTendril;
use html5ever::tokenizer::{BufferQueue, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer};
use html5ever::{local_name, Attribute};

use crate::css::declares_tiny_image_dimension;
use crate::hidden::raw_text_kind;
use crate::tracking::{declares_tiny_dimension, is_known_tracker_host};

#[derive(Default, Debug, Clone)]
pub(crate) struct PreScan {
    /// Exact `src` attribute values (as they appear in the source, before
    /// any sanitizing) that must be force-blocked regardless of
    /// `allow_remote_images`, because they look like a tracking pixel.
    pub(crate) tracker_srcs: HashSet<String>,
    pub(crate) tracking_pixel_count: usize,
    pub(crate) cid_image_count: usize,
    /// Exact CID sources, including duplicates: the resolver subtracts
    /// only references for which it produced a bounded, validated local
    /// image. Kept private because these values are message content.
    pub(crate) cid_image_sources: Vec<String>,
    /// `http(s)` images that are not tracking pixels — only meaningful
    /// (and only counted) when `allow_remote_images` is false, since those
    /// are exactly the ones the sanitizer is about to strip for that
    /// reason.
    pub(crate) remote_image_blocked_count: usize,
}

/// Scan `html` for `<img>` tags and classify each one's `src`.
/// `allow_remote_images` only affects whether a plain remote (non-tracker)
/// image is counted as "blocked" — tracking pixels are always counted and
/// always added to `tracker_srcs`.
pub(crate) fn prescan_images(html: &str, allow_remote_images: bool) -> PreScan {
    let sink = Sink {
        allow_remote_images,
        state: RefCell::new(PreScan::default()),
    };

    let mut chunk = ByteTendril::new();
    chunk.push_slice(html.as_bytes());
    let Ok(str_tendril) = chunk.try_reinterpret() else {
        // `html` is a `&str`, so this is unreachable in practice, but we
        // never want a fallible reinterpret to become a panic — fall back
        // to "nothing observed" instead.
        return PreScan::default();
    };
    let queue = BufferQueue::default();
    queue.push_back(str_tendril);

    let tokenizer = Tokenizer::new(sink, Default::default());
    let _ = tokenizer.feed(&queue);
    tokenizer.end();

    tokenizer.sink.state.into_inner()
}

struct Sink {
    allow_remote_images: bool,
    state: RefCell<PreScan>,
}

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<()> {
        // Same raw-text table as `hidden.rs`/`text.rs`: this pass drives
        // the tokenizer without a tree builder, so nothing else switches
        // it into RAWTEXT for `<style>`/`<script>`. Without it an `<img>`
        // spelled out inside a stylesheet or a script string would be
        // tokenized as a real tag and counted as a blocked remote image
        // the sanitizer is not actually going to remove.
        let Token::TagToken(tag) = &token else {
            return TokenSinkResult::Continue;
        };
        if tag.kind != TagKind::StartTag {
            return TokenSinkResult::Continue;
        }
        if tag.name == local_name!("img") {
            self.observe_img(&tag.attrs);
        }
        match raw_text_kind(&tag.name) {
            Some(kind) => TokenSinkResult::RawData(kind),
            None => TokenSinkResult::Continue,
        }
    }

    fn end(&self) {}
}

impl Sink {
    fn observe_img(&self, attrs: &[Attribute]) {
        let mut src: Option<&str> = None;
        let mut width: Option<&str> = None;
        let mut height: Option<&str> = None;
        let mut style: Option<&str> = None;
        for a in attrs {
            match &*a.name.local {
                "src" => src = Some(a.value.as_ref()),
                "width" => width = Some(a.value.as_ref()),
                "height" => height = Some(a.value.as_ref()),
                "style" => style = Some(a.value.as_ref()),
                _ => {}
            }
        }
        let Some(src) = src else { return };
        if src.trim().is_empty() {
            return;
        }

        let Ok(url) = url::Url::parse(src) else {
            // Not an absolute URL (relative, or unparsable) — the base
            // allow-list denies relative URLs outright (see sanitize.rs),
            // so there is nothing for this report to add for it.
            return;
        };
        let scheme = url.scheme();

        let mut state = self.state.borrow_mut();

        if scheme == "cid" {
            state.cid_image_count += 1;
            state.cid_image_sources.push(src.to_string());
            return;
        }

        if scheme != "http" && scheme != "https" {
            // Any other scheme (javascript:, data:, file:, ...) is killed
            // by the URL-scheme allow-list unconditionally; not this
            // report's concern.
            return;
        }

        let is_tracker = width.is_some_and(declares_tiny_dimension)
            || height.is_some_and(declares_tiny_dimension)
            || style.is_some_and(declares_tiny_image_dimension)
            || url.host_str().is_some_and(is_known_tracker_host);

        if is_tracker {
            state.tracking_pixel_count += 1;
            state.tracker_srcs.insert(src.to_string());
        } else if !self.allow_remote_images {
            state.remote_image_blocked_count += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_img_spelled_out_inside_a_stylesheet_is_not_an_image() {
        let html = r#"<style>.x{}/* <img src="https://cdn.example/a.png"> */</style><script>var s = '<img src="https://cdn.example/b.png">';</script><p>hi</p>"#;
        let scan = prescan_images(html, false);
        assert_eq!(scan.remote_image_blocked_count, 0);
        assert_eq!(scan.tracking_pixel_count, 0);
        assert!(scan.tracker_srcs.is_empty());

        let real = r#"<style>.x{}</style><img src="https://cdn.example/a.png">"#;
        let scan = prescan_images(real, false);
        assert_eq!(scan.remote_image_blocked_count, 1);
    }

    #[test]
    fn tracker_by_dimension_is_flagged_and_counted() {
        let html = r#"<p>hi</p><img src="https://example.com/a.gif" width="1" height="1">"#;
        let scan = prescan_images(html, true);
        assert_eq!(scan.tracking_pixel_count, 1);
        assert_eq!(scan.remote_image_blocked_count, 0);
        assert!(scan.tracker_srcs.contains("https://example.com/a.gif"));
    }

    #[test]
    fn tracker_sized_by_allowlisted_css_is_flagged_and_counted() {
        let html = r#"<img src="https://example.com/a.gif" style="width:1px;height:20px">"#;
        let scan = prescan_images(html, true);
        assert_eq!(scan.tracking_pixel_count, 1);
        assert_eq!(scan.remote_image_blocked_count, 0);
        assert!(scan.tracker_srcs.contains("https://example.com/a.gif"));
    }

    #[test]
    fn tracker_by_known_host_is_flagged_without_dimensions() {
        let html = r#"<img src="https://us1.list-manage.com/track/open.gif">"#;
        let scan = prescan_images(html, true);
        assert_eq!(scan.tracking_pixel_count, 1);
    }

    #[test]
    fn ordinary_remote_image_counted_only_when_disallowed() {
        let html = r#"<img src="https://example.com/photo.jpg" width="640" height="480">"#;
        let allowed = prescan_images(html, true);
        assert_eq!(allowed.remote_image_blocked_count, 0);
        assert_eq!(allowed.tracking_pixel_count, 0);

        let disallowed = prescan_images(html, false);
        assert_eq!(disallowed.remote_image_blocked_count, 1);
        assert_eq!(disallowed.tracking_pixel_count, 0);
    }

    #[test]
    fn cid_image_counted_separately_from_remote() {
        let html = r#"<img src="cid:part1.image@example.com">"#;
        let scan = prescan_images(html, true);
        assert_eq!(scan.cid_image_count, 1);
        assert_eq!(scan.remote_image_blocked_count, 0);
        assert_eq!(scan.tracking_pixel_count, 0);
    }

    #[test]
    fn missing_or_empty_src_is_ignored_not_panicking() {
        let html = r#"<img><img src=""><img src="   ">"#;
        let scan = prescan_images(html, true);
        assert_eq!(scan.tracking_pixel_count, 0);
        assert_eq!(scan.cid_image_count, 0);
        assert_eq!(scan.remote_image_blocked_count, 0);
    }

    #[test]
    fn tracker_wins_over_plain_remote_block_when_both_would_apply() {
        // 1x1 and remote images not allowed: must land in the tracker
        // bucket, not double-counted into remote_image_blocked_count too.
        let html = r#"<img src="https://example.com/a.gif" width="1">"#;
        let scan = prescan_images(html, false);
        assert_eq!(scan.tracking_pixel_count, 1);
        assert_eq!(scan.remote_image_blocked_count, 0);
    }
}
