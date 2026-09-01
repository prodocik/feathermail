//! Drop visually-hidden markup before CSS is narrowed to its safe subset (T-120).
//!
//! T-144 keeps allow-listed presentation CSS, but a newsletter preheader
//! hidden with `font-size:0` can still become visible when only its text is
//! removed from the rendered layout. Hidden intent therefore has to be
//! interpreted while the original inline declaration is still available.
//!
//! This pass is not the security boundary — [`crate::sanitize`] still
//! runs on whatever we emit. It is a presentation pass: if a start tag's
//! own `style` (or the HTML `hidden` attribute) says the subtree is not
//! for human eyes, we omit the subtree instead of un-hiding it. The
//! tokenizer is `html5ever`, the same one ammonia uses, so tag/attribute
//! boundaries agree with the sanitizer. This pass deliberately recognises
//! only the small hidden/preheader vocabulary below; the CSS parser and
//! security allow-list run afterwards in [`crate::sanitize`].

use std::cell::RefCell;

use html5ever::tendril::ByteTendril;
use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer,
};
use html5ever::LocalName;

/// Rewrite `html` so subtrees that the sender hid with inline CSS (or
/// the HTML `hidden` attribute) are absent, instead of becoming visible
/// after [`crate::sanitize`] narrows `style=`.
///
/// Never panics. Unparseable input is returned unchanged so the sanitizer
/// still sees the original bytes.
pub(crate) fn strip_visually_hidden(html: &str) -> String {
    let sink = Sink {
        state: RefCell::new(State::default()),
    };

    let mut chunk = ByteTendril::new();
    chunk.push_slice(html.as_bytes());
    let Ok(str_tendril) = chunk.try_reinterpret() else {
        return html.to_string();
    };
    let queue = BufferQueue::default();
    queue.push_back(str_tendril);

    let tokenizer = Tokenizer::new(sink, Default::default());
    let _ = tokenizer.feed(&queue);
    tokenizer.end();

    let state = tokenizer.sink.state.into_inner();
    if state.seen_text > 0 && state.kept_text == 0 && state.kept_images == 0 {
        // A presentation pass must never leave a reader with a blank
        // message: if everything we recognised turned out to be hidden,
        // the rules were wrong about this sender, not the sender about
        // their mail. The sanitizer still runs on what we return.
        return html.to_string();
    }
    state.out
}

#[derive(Default)]
struct State {
    out: String,
    /// Elements we emitted and have not yet closed, innermost last. A
    /// hidden subtree may never outlive one of them, and the inherited
    /// text flags live here.
    open: Vec<Open>,
    skip: Option<Skip>,
    seen_text: usize,
    kept_text: usize,
    kept_images: usize,
}

/// An open element and what it says about the text inside it.
struct Open {
    name: LocalName,
    tiny_font: bool,
    transparent_color: bool,
}

impl Open {
    fn text_is_invisible(&self) -> bool {
        self.tiny_font || self.transparent_color
    }
}

struct Skip {
    name: LocalName,
    is_phrasing: bool,
    /// Elements opened *inside* the hidden subtree, innermost last.
    inner: Vec<LocalName>,
}

struct Sink {
    state: RefCell<State>,
}

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _line_number: u64) -> TokenSinkResult<()> {
        self.handle(token);
        TokenSinkResult::Continue
    }

    fn end(&self) {}
}

impl Sink {
    fn handle(&self, token: Token) {
        let mut state = self.state.borrow_mut();
        if let Token::CharacterTokens(text) = &token {
            state.seen_text += text.trim().chars().count();
        }
        if state.skip.is_some() && !self.leave_skip(&mut state, &token) {
            return;
        }

        match &token {
            Token::TagToken(tag) => {
                if tag.kind == TagKind::StartTag && start_is_hidden(tag) {
                    if is_void(&tag.name) || tag.self_closing {
                        return;
                    }
                    state.skip = Some(Skip {
                        is_phrasing: is_phrasing(&tag.name),
                        name: tag.name.clone(),
                        inner: Vec::new(),
                    });
                    return;
                }
                track_open(&mut state, tag);
                if tag.kind == TagKind::StartTag && &*tag.name == "img" {
                    state.kept_images += 1;
                }
            }
            Token::CharacterTokens(text) => {
                if text_is_invisible(&state) {
                    return;
                }
                state.kept_text += text.trim().chars().count();
            }
            _ => {}
        }

        emit(&mut state.out, &token);
    }

    /// Returns `true` when the skip has just ended *and* the token still
    /// has to be handled normally; `false` when the token belongs to the
    /// hidden subtree and must be dropped.
    fn leave_skip(&self, state: &mut State, token: &Token) -> bool {
        let Some(skip) = state.skip.as_mut() else {
            return true;
        };
        let Token::TagToken(tag) = token else {
            return false;
        };

        if skip.is_phrasing && skip.inner.is_empty() && closes_phrasing(&tag.name) {
            // HTML5 would have closed the phrasing element before this
            // flow/table tag, so the tag itself is not hidden.
            state.skip = None;
            return true;
        }

        if tag.kind == TagKind::StartTag {
            if !is_void(&tag.name) && !tag.self_closing {
                skip.inner.push(tag.name.clone());
            }
            return false;
        }

        if let Some(pos) = skip.inner.iter().rposition(|n| *n == tag.name) {
            skip.inner.truncate(pos);
            return false;
        }
        if tag.name == skip.name {
            state.skip = None;
            return false;
        }
        if state.open.iter().any(|open| open.name == tag.name) {
            // An ancestor of the hidden element is closing, so the hidden
            // element ended with it however unbalanced the markup was.
            // Without this a single stray `<td style="display:none">`
            // would swallow the rest of the letter (T-142).
            state.skip = None;
            return true;
        }
        false
    }
}

fn text_is_invisible(state: &State) -> bool {
    state
        .open
        .last()
        .map(Open::text_is_invisible)
        .unwrap_or(false)
}

fn track_open(state: &mut State, tag: &Tag) {
    match tag.kind {
        TagKind::StartTag => {
            if closes_phrasing(&tag.name) {
                while state
                    .open
                    .last()
                    .is_some_and(|open| is_phrasing(&open.name))
                {
                    state.open.pop();
                }
            }
            if is_void(&tag.name) || tag.self_closing {
                return;
            }
            let (mut tiny_font, mut transparent_color) = state
                .open
                .last()
                .map(|open| (open.tiny_font, open.transparent_color))
                .unwrap_or((false, false));
            if let Some(style) = style_of(tag) {
                if let Some(zero) = font_size_is_zero(style) {
                    tiny_font = zero;
                }
                if let Some(transparent) = color_is_transparent(style) {
                    transparent_color = transparent;
                }
            }
            state.open.push(Open {
                name: tag.name.clone(),
                tiny_font,
                transparent_color,
            });
        }
        TagKind::EndTag => {
            if let Some(pos) = state.open.iter().rposition(|open| open.name == tag.name) {
                state.open.truncate(pos);
            }
        }
    }
}

fn style_of(tag: &Tag) -> Option<&str> {
    tag.attrs
        .iter()
        .find(|a| &*a.name.local == "style")
        .map(|a| a.value.as_ref())
}

fn start_is_hidden(tag: &Tag) -> bool {
    for a in &tag.attrs {
        match &*a.name.local {
            "hidden" => return true,
            "style" if style_hides(a.value.as_ref()) => return true,
            _ => {}
        }
    }
    false
}

/// Declarations of a `style=` attribute, lowercase-comparable, with
/// `!important` already stripped from the value.
fn declarations(style: &str) -> impl Iterator<Item = (&str, &str)> {
    style.split(';').filter_map(|decl| {
        let (name, value) = decl.split_once(':')?;
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        Some((name, strip_important(value.trim())))
    })
}

/// Box-level hiding: the element and everything under it is out of the
/// layout, images included, so the subtree can go.
///
/// Conservative, property-by-property. A full CSS parser would be a new
/// security surface; we only recognise the declarations newsletters use
/// to hide a preheader, after lowercasing and stripping `!important`.
fn style_hides(style: &str) -> bool {
    let mut max_height_zero = false;
    let mut overflow_hidden = false;
    for (name, value) in declarations(style) {
        if name.eq_ignore_ascii_case("display") && eq_ascii(value, "none") {
            return true;
        }
        if name.eq_ignore_ascii_case("visibility") && eq_ascii(value, "hidden") {
            return true;
        }
        if name.eq_ignore_ascii_case("opacity") && is_zero_number(value) {
            return true;
        }
        if name.eq_ignore_ascii_case("mso-hide") && eq_ascii(value, "all") {
            return true;
        }
        if name.eq_ignore_ascii_case("max-height") && is_zero_length(value) {
            max_height_zero = true;
        }
        if (name.eq_ignore_ascii_case("overflow") || name.eq_ignore_ascii_case("overflow-y"))
            && eq_ascii(value, "hidden")
        {
            overflow_hidden = true;
        }
    }
    max_height_zero && overflow_hidden
}

/// `Some(true)` when this element sets a zero font size, `Some(false)`
/// when it sets a readable one, `None` when it says nothing and the
/// value is inherited.
///
/// `font-size:0` is *not* a box-level signal: MJML puts it on every
/// wrapper it generates to kill the whitespace between inline blocks,
/// and the children set their own size. Reading it as "this subtree is
/// hidden" ate whole letters (T-142). It hides text and nothing else,
/// exactly as CSS says, so it travels down as inheritance and stops at
/// the first descendant that sets a size of its own.
fn font_size_is_zero(style: &str) -> Option<bool> {
    let mut answer = None;
    for (name, value) in declarations(style) {
        if name.eq_ignore_ascii_case("font-size") {
            answer = Some(is_zero_length(value));
        }
    }
    answer
}

/// Same inheritance story for text painted in nothing at all.
fn color_is_transparent(style: &str) -> Option<bool> {
    let mut answer = None;
    for (name, value) in declarations(style) {
        if name.eq_ignore_ascii_case("color") {
            answer = Some(eq_ascii(value, "transparent"));
        }
    }
    answer
}

fn strip_important(value: &str) -> &str {
    let trimmed = value.trim();
    const MARKER: &str = "!important";
    if trimmed.len() >= MARKER.len()
        && trimmed.as_bytes()[trimmed.len() - MARKER.len()..]
            .eq_ignore_ascii_case(MARKER.as_bytes())
    {
        return trimmed[..trimmed.len() - MARKER.len()].trim();
    }
    trimmed
}

fn eq_ascii(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn is_zero_number(value: &str) -> bool {
    let v = value.trim();
    let num = v.trim_end_matches('%').trim();
    parse_zero(num)
}

fn is_zero_length(value: &str) -> bool {
    let v = value.trim();
    let num = v.trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%');
    if num.len() == v.len()
        && !v
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '+' || c == '-')
    {
        // `0 1px` / `inherit` / `smaller` — not a single length.
        return false;
    }
    parse_zero(num.trim())
}

fn parse_zero(num: &str) -> bool {
    if num.is_empty() {
        return false;
    }
    match num.parse::<f32>() {
        Ok(n) => n == 0.0,
        Err(_) => false,
    }
}

fn is_void(name: &LocalName) -> bool {
    matches!(
        &**name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_phrasing(name: &LocalName) -> bool {
    matches!(
        &**name,
        "span"
            | "font"
            | "b"
            | "i"
            | "u"
            | "em"
            | "strong"
            | "a"
            | "small"
            | "abbr"
            | "acronym"
            | "code"
            | "tt"
            | "sub"
            | "sup"
            | "strike"
            | "s"
            | "q"
            | "mark"
            | "time"
            | "data"
            | "var"
            | "kbd"
            | "samp"
            | "cite"
            | "dfn"
            | "bdi"
            | "bdo"
            | "nobr"
    )
}

/// Start *or* end of these tags closes an unclosed phrasing element
/// under HTML5 reconstruction. Without this, a tokenizer-only depth
/// counter would treat `<span style="font-size:0">x<table>` as "the
/// table is inside the span" and drop the rest of the message.
fn closes_phrasing(name: &LocalName) -> bool {
    matches!(
        &**name,
        "div"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "td"
            | "th"
            | "caption"
            | "colgroup"
            | "col"
            | "p"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "li"
            | "dl"
            | "dt"
            | "dd"
            | "hr"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "aside"
            | "nav"
            | "figure"
            | "figcaption"
            | "blockquote"
            | "pre"
            | "center"
            | "address"
    )
}

fn emit(out: &mut String, token: &Token) {
    match token {
        Token::TagToken(tag) => emit_tag(out, tag),
        Token::CharacterTokens(text) => push_escaped_text(out, text),
        Token::NullCharacterToken => {}
        Token::CommentToken(_)
        | Token::DoctypeToken(_)
        | Token::EOFToken
        | Token::ParseError(_) => {}
    }
}

fn emit_tag(out: &mut String, tag: &Tag) {
    out.push('<');
    if tag.kind == TagKind::EndTag {
        out.push('/');
        out.push_str(&tag.name);
        out.push('>');
        return;
    }
    out.push_str(&tag.name);
    for a in &tag.attrs {
        out.push(' ');
        out.push_str(&a.name.local);
        out.push_str("=\"");
        push_escaped_attr(out, &a.value);
        out.push('"');
    }
    if tag.self_closing {
        out.push_str(" /");
    }
    out.push('>');
}

fn push_escaped_text(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

fn push_escaped_attr(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            _ => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_zero_preheader_is_dropped_and_the_body_stays() {
        let raw = concat!(
            "<p>Hello</p>",
            r#"<span style="font-size:0px;line-height:0px">PREHEADER_TOKEN</span>"#,
            "<p>World</p>",
        );
        let out = strip_visually_hidden(raw);
        assert!(!out.contains("PREHEADER_TOKEN"), "{out}");
        assert!(out.contains("Hello"), "{out}");
        assert!(out.contains("World"), "{out}");
    }

    #[test]
    fn display_none_block_is_dropped() {
        let raw = r#"<div style="display:none">HIDDEN_TOKEN</div><p>VISIBLE_TOKEN</p>"#;
        let out = strip_visually_hidden(raw);
        assert!(!out.contains("HIDDEN_TOKEN"), "{out}");
        assert!(out.contains("VISIBLE_TOKEN"), "{out}");
    }

    #[test]
    fn unclosed_phrasing_preheader_does_not_eat_the_following_table() {
        let raw = concat!(
            r#"<span style="font-size:0px">PREHEADER_TOKEN"#,
            "<table><tr><td>BODY_TOKEN</td></tr></table>",
        );
        let out = strip_visually_hidden(raw);
        assert!(!out.contains("PREHEADER_TOKEN"), "{out}");
        assert!(out.contains("BODY_TOKEN"), "{out}");
    }

    #[test]
    fn ordinary_font_size_is_kept() {
        let raw = r#"<p style="font-size:16px">KEEP_TOKEN</p>"#;
        let out = strip_visually_hidden(raw);
        assert!(out.contains("KEEP_TOKEN"), "{out}");
    }

    #[test]
    fn html_hidden_attribute_drops_the_subtree() {
        let raw = "<p hidden>HIDDEN_TOKEN</p><p>VISIBLE_TOKEN</p>";
        let out = strip_visually_hidden(raw);
        assert!(!out.contains("HIDDEN_TOKEN"), "{out}");
        assert!(out.contains("VISIBLE_TOKEN"), "{out}");
    }

    #[test]
    fn opacity_zero_and_transparent_color_drop() {
        let a = r#"<span style="opacity:0">A_TOKEN</span>keep"#;
        let b = r#"<span style="color:transparent">B_TOKEN</span>keep"#;
        let c = r#"<td style="max-height:0;overflow:hidden">C_TOKEN</td>keep"#;
        for (raw, token) in [(a, "A_TOKEN"), (b, "B_TOKEN"), (c, "C_TOKEN")] {
            let out = strip_visually_hidden(raw);
            assert!(!out.contains(token), "{token} survived in {out}");
            assert!(out.contains("keep"), "{out}");
        }
    }

    #[test]
    fn a_layout_cell_with_font_size_zero_keeps_the_letter() {
        // MJML generates exactly this: `font-size:0px` on the wrapper to
        // kill the whitespace between inline blocks, real sizes on the
        // children. T-142 — reading it as "hidden" ate whole letters.
        let raw = concat!(
            r#"<table><tr><td style="direction:ltr;font-size:0px;padding:0;text-align:center;">"#,
            r#"<div style="font-size:0px;display:inline-block;width:100%">"#,
            r#"<div style="font-size:13px;line-height:1.5">BODY_TOKEN</div>"#,
            r#"<img src="cid:logo" alt="LOGO_TOKEN">"#,
            "</div></td></tr></table>",
        );
        let out = strip_visually_hidden(raw);
        assert!(out.contains("BODY_TOKEN"), "{out}");
        assert!(out.contains("LOGO_TOKEN"), "{out}");
    }

    #[test]
    fn zero_font_size_travels_down_until_a_child_sets_its_own() {
        let raw = concat!(
            r#"<div style="font-size:0"><span>PREHEADER_TOKEN</span>"#,
            r#"<p style="font-size:14px">BODY_TOKEN</p></div>"#,
        );
        let out = strip_visually_hidden(raw);
        assert!(!out.contains("PREHEADER_TOKEN"), "{out}");
        assert!(out.contains("BODY_TOKEN"), "{out}");
    }

    #[test]
    fn a_hidden_subtree_never_outlives_its_parent() {
        // The sender never closes the hidden cell. Counting `</td>` alone
        // would swallow the rest of the message (T-142); the enclosing
        // `</tr>` says the cell is over.
        let raw = concat!(
            "<table><tr>",
            r#"<td style="display:none">HIDDEN_TOKEN<td>"#,
            "</tr><tr><td>BODY_TOKEN</td></tr></table>",
        );
        let out = strip_visually_hidden(raw);
        assert!(!out.contains("HIDDEN_TOKEN"), "{out}");
        assert!(out.contains("BODY_TOKEN"), "{out}");
    }

    #[test]
    fn a_letter_that_would_come_out_blank_is_handed_over_untouched() {
        let raw = r#"<div style="display:none">ONLY_TOKEN</div>"#;
        let out = strip_visually_hidden(raw);
        assert!(out.contains("ONLY_TOKEN"), "a blank body is worse: {out}");
    }

    #[test]
    fn an_image_only_letter_still_loses_its_hidden_preheader() {
        let raw = concat!(
            r#"<div style="display:none">PREHEADER_TOKEN</div>"#,
            r#"<img src="cid:hero" alt="ART_TOKEN">"#,
        );
        let out = strip_visually_hidden(raw);
        assert!(!out.contains("PREHEADER_TOKEN"), "{out}");
        assert!(out.contains("ART_TOKEN"), "{out}");
    }

    #[test]
    fn important_font_size_zero_still_drops() {
        let raw = r#"<span style="font-size: 0px !important">PREHEADER_TOKEN</span>keep"#;
        let out = strip_visually_hidden(raw);
        assert!(!out.contains("PREHEADER_TOKEN"), "{out}");
        assert!(out.contains("keep"), "{out}");
    }
}
