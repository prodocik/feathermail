//! Plain text out of an HTML body, for the search index (T-096).
//!
//! This is **not** a display renderer and must never be used as one — the
//! only renderer allowed to show a message is the WebKitGTK widget in
//! `crates/app/src/html_view.rs`, and the only thing safe to hand it is a
//! [`crate::SanitizedHtml`]. What this module produces is index fodder:
//! the words a message says, whitespace-collapsed onto one line, bound as
//! a SQL parameter into the `body` column of `messages_fts` and read back
//! only by FTS5's tokenizer.
//!
//! Why it exists: until now an HTML-only message (no `text/plain`
//! alternative) was indexed with an empty body, so it was findable by
//! sender, subject, labels and attachment names but never by a word it
//! actually contained. T-093 wrote that gap down rather than closing it,
//! because at the time the message panel could not show HTML either, and
//! indexing text nobody could see would have been a promise with no
//! display-side counterpart. T-030 gave HTML mail a real renderer, so the
//! counterpart now exists and the gap is a plain bug.
//!
//! **Why the tokenizer and not the tree.** Text extraction needs a stream
//! of character tokens, not a DOM: there is no query to answer about
//! structure, only "which characters would a reader see". Driving
//! `html5ever`'s tokenizer directly avoids building (and keeping alive) a
//! whole tree for a body that may be megabytes of table markup. It does
//! mean this module has to do by hand the one thing the tree builder would
//! otherwise do for it — put the tokenizer into the right raw-text state
//! for `<script>`, `<style>` and friends — and [`raw_text_kind`] is a
//! deliberate copy of that table, so an element whose content is *not*
//! markup is never re-tokenized as if it were.

use std::cell::{Cell, RefCell};

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};

use crate::sanitize::DEFAULT_MAX_INPUT_BYTES;
use crate::UnsanitizedHtml;

/// The words an HTML body says, collapsed onto a single space-separated
/// line, for the search index. Never panics and never returns an error:
/// the tokenizer tolerates any byte sequence, and everything this module
/// does with the tokens is pushing characters into a `String`.
///
/// Input larger than [`DEFAULT_MAX_INPUT_BYTES`] returns an empty string
/// rather than a truncated one, for the same reason [`crate::sanitize`]
/// refuses it: a body the renderer will not show has no display-side
/// counterpart for the index to be honest against, and refusing is a flat
/// length check that a hostile giant body cannot turn into CPU. Such a
/// message stays findable by sender, subject, labels and attachment names.
///
/// D14: the returned `String` is message content. It must never be logged,
/// `Debug`-printed, or put in an error message — it has exactly one
/// destination, and that destination is a bound SQL parameter.
pub fn text_for_search(html: &UnsanitizedHtml) -> String {
    let raw = html.as_unsanitized_str();
    if raw.len() > DEFAULT_MAX_INPUT_BYTES {
        return String::new();
    }

    let tokenizer = Tokenizer::new(TextSink::default(), TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(raw));
    // The sink never asks for a script to run, so one `feed` consumes the
    // whole buffer; `end` flushes whatever the tokenizer was still holding.
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    tokenizer.sink.out.into_inner()
}

/// Elements whose content is text but not *message* text: markup a reader
/// never sees. Their characters are dropped, not separated — a CSS rule or
/// a script body has no words worth searching, and indexing them is how a
/// search for "font" starts matching every newsletter ever sent.
///
/// `<noscript>` is deliberately absent: the renderer runs with JavaScript
/// off (T-030), so its content is exactly what a reader sees.
fn is_dropped(name: &str) -> bool {
    matches!(
        name,
        "script" | "style" | "title" | "iframe" | "noembed" | "noframes"
    )
}

/// The tree builder's raw-text table (`html5ever::tree_builder`), copied
/// here because this module drives the tokenizer without a tree builder to
/// set these states for it. Without this, a `<style>` block containing
/// `a > b` or a script containing `if (x < y)` would be tokenized as tags,
/// and the "drop this element's content" bookkeeping below would come
/// apart on markup that is not markup.
///
/// `<textarea>` and `<xmp>` are listed because the tokenizer needs the
/// state, even though their content *is* shown to a reader and therefore
/// is not dropped.
fn raw_text_kind(name: &str) -> Option<RawKind> {
    match name {
        "script" => Some(RawKind::ScriptData),
        "style" | "iframe" | "noembed" | "noframes" | "xmp" => Some(RawKind::Rawtext),
        "title" | "textarea" => Some(RawKind::Rcdata),
        _ => None,
    }
}

/// Elements that do not break a word. Everything else — including every
/// tag this list has never heard of — separates, because the failure it
/// prevents is worse than the one it causes: `<td>Ivan</td><td>Petrov</td>`
/// glued into one token is a word that exists in no message and matches no
/// query, while an unknown tag splitting one word into two still leaves
/// both halves searchable.
fn is_inline(name: &str) -> bool {
    matches!(
        name,
        "a" | "abbr"
            | "b"
            | "bdi"
            | "bdo"
            | "big"
            | "cite"
            | "code"
            | "data"
            | "dfn"
            | "em"
            | "font"
            | "i"
            | "kbd"
            | "mark"
            | "nobr"
            | "q"
            | "s"
            | "samp"
            | "small"
            | "span"
            | "strike"
            | "strong"
            | "sub"
            | "sup"
            | "time"
            | "tt"
            | "u"
            | "var"
            | "wbr"
    )
}

/// Collects character tokens into one whitespace-collapsed line.
///
/// `TokenSink::process_token` takes `&self`, so the state is in cells. The
/// separator is *pending* rather than pushed eagerly: that is what makes
/// `<span>a</span><span>b</span>` come out as `ab` (no separator, no
/// whitespace between the runs) while `<p>a</p><p>b</p>` comes out as
/// `a b`, and it is why no leading or doubled space can ever reach the
/// output.
#[derive(Default)]
struct TextSink {
    out: RefCell<String>,
    /// Nesting depth inside [`is_dropped`] elements. A `<script>` that is
    /// never closed swallows the rest of the document — which is exactly
    /// what a browser does with it, so the index agrees with the panel.
    dropped_depth: Cell<u32>,
    pending_space: Cell<bool>,
}

impl TextSink {
    fn separate(&self) {
        if !self.out.borrow().is_empty() {
            self.pending_space.set(true);
        }
    }

    fn push_text(&self, text: &str) {
        if self.dropped_depth.get() > 0 {
            return;
        }
        let mut out = self.out.borrow_mut();
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !out.is_empty() {
                    self.pending_space.set(true);
                }
            } else {
                if self.pending_space.replace(false) {
                    out.push(' ');
                }
                out.push(ch);
            }
        }
    }

    fn push_tag(&self, tag: Tag) -> TokenSinkResult<()> {
        let name: &str = &tag.name;
        if !is_inline(name) {
            self.separate();
        }
        match tag.kind {
            // `self_closing` is deliberately ignored. For every element
            // named here the solidus in `<script/>` is a parse error the
            // HTML5 spec tells a parser to drop on the floor, so the
            // element is open and its content is raw text -- treating it
            // as closed is how a `<style/>` ends up indexing its own CSS.
            TagKind::StartTag => {
                if is_dropped(name) {
                    self.dropped_depth.set(self.dropped_depth.get() + 1);
                }
                if let Some(kind) = raw_text_kind(name) {
                    return TokenSinkResult::RawData(kind);
                }
            }
            TagKind::EndTag => {
                if is_dropped(name) {
                    self.dropped_depth
                        .set(self.dropped_depth.get().saturating_sub(1));
                }
            }
        }
        TokenSinkResult::Continue
    }
}

impl TokenSink for TextSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<()> {
        match token {
            Token::TagToken(tag) => self.push_tag(tag),
            Token::CharacterTokens(text) => {
                self.push_text(&text);
                TokenSinkResult::Continue
            }
            // Comments, doctypes, stray NULs and the tokenizer's own parse
            // errors carry nothing a reader sees. Dropping comments is not
            // cosmetic: conditional comments are where mail clients hide
            // whole alternative layouts, and indexing both would make one
            // message match twice for words it shows once.
            Token::CommentToken(_)
            | Token::DoctypeToken(_)
            | Token::NullCharacterToken
            | Token::ParseError(_)
            | Token::EOFToken => TokenSinkResult::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::text_for_search;
    use crate::sanitize::DEFAULT_MAX_INPUT_BYTES;
    use crate::UnsanitizedHtml;

    fn text(html: &str) -> String {
        text_for_search(&UnsanitizedHtml::for_test(html.to_string()))
    }

    #[test]
    fn an_html_only_body_yields_the_words_a_reader_sees() {
        let out =
            text("<html><body><p>Договор подписан</p><p>Ждём сканы до пятницы.</p></body></html>");
        assert_eq!(out, "Договор подписан Ждём сканы до пятницы.");
    }

    #[test]
    fn style_and_script_content_never_reaches_the_index() {
        // The exact failure this whole module is here to avoid: CSS
        // property names and JS identifiers are not words the sender wrote.
        let out = text(concat!(
            "<head><style>.wrapper { font-family: Helvetica; color: #333 }</style></head>",
            "<body><script>var tracker = 'pixel';</script><p>Invoice attached</p></body>",
        ));
        assert_eq!(out, "Invoice attached");
        assert!(!out.contains("font-family"));
        assert!(!out.contains("Helvetica"));
        assert!(!out.contains("tracker"));
    }

    #[test]
    fn a_title_is_markup_not_message_text() {
        // `<title>` is chrome, not body: the panel shows it nowhere, so
        // indexing it would make a message match a word it never displays.
        let out = text("<head><title>Newsletter template v4</title></head><body>Hello</body>");
        assert_eq!(out, "Hello");
    }

    #[test]
    fn block_boundaries_separate_words_and_inline_ones_do_not() {
        // The two halves of the same rule. Table cells are how real mail
        // lays out everything, and gluing them would invent words.
        assert_eq!(
            text("<table><tr><td>Ivan</td><td>Petrov</td></tr></table>"),
            "Ivan Petrov"
        );
        assert_eq!(text("<p>Ivan<br>Petrov</p>"), "Ivan Petrov");
        // ...while a span in the middle of a word must not split it: this
        // is exactly how bold-the-first-letter markup is written.
        assert_eq!(text("<p><b>I</b>van</p>"), "Ivan");
        assert_eq!(text("<p><span>Iv</span><span>an</span></p>"), "Ivan");
    }

    #[test]
    fn character_references_are_decoded_and_collapse_like_whitespace() {
        assert_eq!(text("<p>Tom &amp; Jerry</p>"), "Tom & Jerry");
        assert_eq!(text("<p>&lt;not a tag&gt;</p>"), "<not a tag>");
        // `&nbsp;` is U+00A0, whitespace as far as a reader and a search
        // index are concerned -- it must not weld two words together.
        assert_eq!(
            text("<p>Total:&nbsp;&nbsp;1200&nbsp;₽</p>"),
            "Total: 1200 ₽"
        );
    }

    #[test]
    fn whitespace_is_collapsed_without_a_leading_or_doubled_space() {
        let out = text("   <p>\n\n   Hello   \t world \n</p>   <p>  </p>   ");
        assert_eq!(out, "Hello world");
        assert!(!out.starts_with(' '));
        assert!(!out.ends_with(' '));
        assert!(!out.contains("  "));
    }

    #[test]
    fn a_style_body_that_looks_like_markup_is_not_tokenized_as_tags() {
        // Without the raw-text state this module copies from the tree
        // builder, `a > b` inside CSS and `x < y` inside a script are read
        // as tags, and the "drop this element" bookkeeping comes apart on
        // markup that was never markup.
        let out = text(concat!(
            "<style>.a > .b { content: \"<p>ghost</p>\" }</style>",
            "<script>if (x < y) { document.write('<div>ghost</div>'); }</script>",
            "<p>real</p>",
        ));
        assert_eq!(out, "real");
    }

    #[test]
    fn a_script_that_writes_markup_does_not_desynchronise_the_drop_counter() {
        // The reason `raw_text_kind` copies the tree builder's table.
        // Inside script data a `<script>` in a string literal is text, not
        // a tag; tokenized as a tag it would push the drop counter to 2,
        // the real `</script>` would only bring it back to 1, and every
        // word after it would be silently dropped. Inline scripts that
        // build markup out of strings are ordinary in HTML mail.
        let out = text("<script>var s = \"<script>\";</script><p>real</p>");
        assert_eq!(out, "real");
    }

    #[test]
    fn textarea_content_is_literal_text_not_markup() {
        // The other half of `raw_text_kind`: `<textarea>` is RCDATA, so a
        // reader sees the angle brackets. Tokenizing its content as markup
        // would quietly eat them.
        assert_eq!(
            text("<textarea>3 < 5 and <b>x</b></textarea>"),
            "3 < 5 and <b>x</b>"
        );
    }

    #[test]
    fn a_self_closed_style_still_swallows_its_own_css() {
        // `<style/>` is a parse error the HTML5 spec says to ignore: the
        // element is open. Honouring the solidus would index the CSS.
        let out = text("<p>hi</p><style/>p{color:red}</style><p>bye</p>");
        assert_eq!(out, "hi bye");
    }

    #[test]
    fn an_unclosed_script_swallows_the_rest_exactly_as_a_browser_does() {
        // Not a happy outcome, but the honest one: WebKit shows nothing
        // after this point either, so the index and the panel agree.
        let out = text("<p>before</p><script>var a = 1;<p>after</p>");
        assert_eq!(out, "before");
    }

    #[test]
    fn comments_are_not_indexed() {
        // Conditional comments carry whole alternative layouts for other
        // mail clients; indexing them makes one message match twice for
        // words it shows once.
        let out = text("<p>shown</p><!--[if mso]><p>outlook only</p><![endif]-->");
        assert_eq!(out, "shown");
    }

    #[test]
    fn noscript_and_textarea_text_is_kept_because_a_reader_sees_it() {
        // The renderer runs with JavaScript off (T-030), so `<noscript>`
        // is the branch that is actually displayed.
        assert_eq!(
            text("<noscript><p>Enable images to see this</p></noscript>"),
            "Enable images to see this"
        );
        assert_eq!(text("<textarea>draft reply</textarea>"), "draft reply");
    }

    #[test]
    fn attribute_values_are_not_body_text() {
        // A link's href and an image's alt/tracking parameters are not
        // words the sender wrote; only the anchor text is.
        let out = text(
            "<a href=\"https://tracker.example/click?campaign=summer\" title=\"promo\">Open</a>",
        );
        assert_eq!(out, "Open");
    }

    #[test]
    fn oversized_html_indexes_as_nothing_rather_than_as_a_prefix() {
        let mut html = String::with_capacity(DEFAULT_MAX_INPUT_BYTES + 64);
        html.push_str("<p>needle ");
        while html.len() <= DEFAULT_MAX_INPUT_BYTES {
            html.push('a');
        }
        html.push_str("</p>");
        assert!(text_for_search(&UnsanitizedHtml::for_test(html)).is_empty());
    }

    #[test]
    fn an_html_body_at_the_size_limit_is_still_indexed() {
        // The other side of the same boundary: the refusal must be the
        // documented limit, not "anything biggish".
        let filler = "a".repeat(DEFAULT_MAX_INPUT_BYTES - "<p>needle </p>".len());
        let html = format!("<p>needle {filler}</p>");
        assert_eq!(html.len(), DEFAULT_MAX_INPUT_BYTES);
        let out = text_for_search(&UnsanitizedHtml::for_test(html));
        assert!(out.starts_with("needle "));
    }

    #[test]
    fn fuzz_random_bytes_never_panics() {
        // Same deterministic xorshift as `select.rs` uses: no external RNG
        // dependency is allowed in this crate.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut buf = String::with_capacity(8192);
        for _ in 0..8192 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            // Keep it valid UTF-8 -- `UnsanitizedHtml` always holds a
            // `String`, so invalid bytes cannot reach this function.
            buf.push(char::from_u32((state & 0x7FF) as u32).unwrap_or('?'));
        }
        let _ = text_for_search(&UnsanitizedHtml::for_test(buf));
    }

    #[test]
    fn structural_fuzz_mutated_html_never_panics_and_never_leaks_css() {
        // Damage a real newsletter-shaped body the way truncated cache
        // files and bad fetches damage one, and assert the two properties
        // that must survive any corruption: no panic, and no leading or
        // doubled space in the output.
        let base = concat!(
            "<html><head><style>.b { color: #fff }</style></head>",
            "<body><table><tr><td><p>Order <b>1042</b> shipped</p></td>",
            "<td><a href=\"https://x.example\">Track</a></td></tr></table>",
            "<script>var t = 0;</script></body></html>",
        );
        let mut state: u64 = 0x243F6A8885A308D3;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let mut bytes = base.as_bytes().to_vec();
            match next() % 4 {
                0 => bytes.truncate((next() as usize) % base.len()),
                1 => {
                    let at = (next() as usize) % base.len();
                    bytes[at] = (next() & 0x7F) as u8;
                }
                2 => {
                    let at = (next() as usize) % base.len();
                    let len = ((next() as usize) % 32).min(bytes.len() - at);
                    bytes.drain(at..at + len);
                }
                _ => {
                    let at = (next() as usize) % base.len();
                    bytes.splice(at..at, b"<<>\"&#x".iter().copied());
                }
            }
            let damaged = String::from_utf8_lossy(&bytes).into_owned();
            let out = text_for_search(&UnsanitizedHtml::for_test(damaged));
            assert!(!out.starts_with(' '));
            assert!(!out.contains("  "));
        }
    }
}
