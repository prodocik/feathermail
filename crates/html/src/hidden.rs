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
use html5ever::tokenizer::states::RawKind;
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
    /// The open raw-text element, if any (see [`raw_text_kind`]). Its
    /// content is literal text: it is copied out verbatim, and it is not
    /// text a reader sees, so it counts towards neither `seen_text` nor
    /// `kept_text`.
    raw_text: Option<LocalName>,
    seen_text: usize,
    kept_text: usize,
    kept_images: usize,
}

/// The tree builder's raw-text table, copied here for the same reason
/// `text.rs` copies it: this pass drives the tokenizer without a tree
/// builder, so nothing else puts the tokenizer into RAWTEXT/RCDATA. Without
/// it, `<style>.a > .b{}</style>` arrives as ordinary character tokens and
/// leaves here as `.a &gt; .b{}` — which ammonia keeps verbatim (raw text
/// is not entity-decoded) and `css::sanitize_selector` then rejects,
/// silently dropping the sender's stylesheet.
pub(crate) fn raw_text_kind(name: &str) -> Option<RawKind> {
    match name {
        "script" => Some(RawKind::ScriptData),
        "style" | "iframe" | "noembed" | "noframes" | "xmp" => Some(RawKind::Rawtext),
        "title" | "textarea" => Some(RawKind::Rcdata),
        _ => None,
    }
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
        // Decided from the token itself and returned unconditionally, even
        // for a start tag inside a hidden subtree: the tokenizer state has
        // to match the document either way, or `<style>x<p>y</style>` would
        // push a phantom `<p>` onto the skip stack and swallow the rest of
        // the letter. `self_closing` is deliberately ignored — `<style/>`
        // is a parse error HTML5 says to drop, the element stays open.
        let raw = match &token {
            Token::TagToken(tag) if tag.kind == TagKind::StartTag => raw_text_kind(&tag.name),
            _ => None,
        };
        self.handle(token);
        match raw {
            Some(kind) => TokenSinkResult::RawData(kind),
            None => TokenSinkResult::Continue,
        }
    }

    fn end(&self) {}
}

impl Sink {
    fn handle(&self, token: Token) {
        let mut state = self.state.borrow_mut();
        let in_raw_text = state.raw_text.is_some();
        if let Token::TagToken(tag) = &token {
            if raw_text_kind(&tag.name).is_some() {
                match tag.kind {
                    TagKind::StartTag => state.raw_text = Some(tag.name.clone()),
                    TagKind::EndTag if state.raw_text.as_ref() == Some(&tag.name) => {
                        state.raw_text = None;
                    }
                    TagKind::EndTag => {}
                }
            }
        }
        if let Token::CharacterTokens(text) = &token {
            if !in_raw_text {
                state.seen_text += text.trim().chars().count();
            }
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
            // Raw-text content is skipped here on purpose: a stylesheet is
            // neither hideable by the font-size rules nor evidence that the
            // letter still has something for a reader to see.
            Token::CharacterTokens(text) if !in_raw_text => {
                if text_is_invisible(&state) {
                    return;
                }
                state.kept_text += text.trim().chars().count();
            }
            _ => {}
        }

        emit(&mut state.out, &token, in_raw_text);
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

fn emit(out: &mut String, token: &Token, in_raw_text: bool) {
    match token {
        Token::TagToken(tag) => emit_tag(out, tag),
        // Raw-text content is literal, not markup: escaping `>`/`&` here
        // corrupts the CSS (or script) that the next pass has to parse.
        Token::CharacterTokens(text) if in_raw_text => out.push_str(text),
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

    #[test]
    fn raw_text_in_a_style_block_is_copied_verbatim() {
        // Escaping `>`/`&` here corrupts the stylesheet: ammonia keeps raw
        // text as-is (it decodes no entities inside `<style>`), so the CSS
        // parser downstream would see `.a &gt; .b` and drop the rule.
        let out = strip_visually_hidden("<style>.a > .b{color:red}</style><p>hi</p>");
        assert!(out.contains(".a > .b"), "{out}");
        let out = strip_visually_hidden("<style>.a{font-family:\"A&B\"}</style><p>hi</p>");
        assert!(out.contains("\"A&B\""), "{out}");
    }

    #[test]
    fn markup_inside_raw_text_does_not_desynchronise_the_hidden_bookkeeping() {
        // Without the raw-text states, `<p>` inside a script string is a
        // real start tag to this tokenizer; opened inside a hidden subtree
        // it would never be closed and would swallow the rest of the mail.
        let raw = concat!(
            r#"<div style="display:none"><script>var s = "<p>";</script></div>"#,
            "<p>Important message text</p>"
        );
        let out = strip_visually_hidden(raw);
        assert!(out.contains("Important message text"), "{out}");
    }

    #[test]
    fn a_stylesheet_does_not_count_as_the_text_the_all_hidden_guard_looks_for() {
        // A stylesheet is not prose. If its content counted as kept text,
        // a letter whose *only* real text was hidden would look like it
        // still had something to read, and the "everything we recognised
        // turned out hidden -- give the reader the original" guard would
        // never fire.
        let raw = concat!(
            "<style>.a{color:red}.b{color:blue}</style>",
            r#"<span style="display:none">PREHEADER_TOKEN</span>"#
        );
        assert_eq!(strip_visually_hidden(raw), raw);

        // ... and with real visible text present the guard still stands
        // down, stylesheet or no stylesheet.
        let raw = concat!(
            "<style>.a > .b{color:red}</style>",
            r#"<span style="display:none">PREHEADER_TOKEN</span>"#,
            "<p>Visible</p>"
        );
        let out = strip_visually_hidden(raw);
        assert!(!out.contains("PREHEADER_TOKEN"), "{out}");
        assert!(out.contains(".a > .b"), "{out}");
        assert!(out.contains("Visible"), "{out}");
    }
}
