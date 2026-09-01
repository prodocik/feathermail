//! Safe CSS subset for HTML mail (T-144).
//!
//! CSS is parsed, not searched with regular expressions.  The allow-list is
//! deliberately narrower than browser CSS: mail may control typography,
//! spacing, tables and responsive layout inside its own body, but it may not
//! fetch a resource, create an overlay, generate content or run animation.

use cssparser::{
    AtRuleParser, BasicParseErrorKind, CowRcStr, DeclarationParser, ParseError, Parser,
    ParserInput, ParserState, QualifiedRuleParser, RuleBodyItemParser, RuleBodyParser,
    StyleSheetParser, ToCss, Token, TokenSerializationType,
};

use crate::tracking::declares_tiny_dimension;

const MAX_RULE_NESTING: usize = 4;

const SAFE_PROPERTIES: &[&str] = &[
    "align-content",
    "align-items",
    "align-self",
    "background",
    "background-clip",
    "background-color",
    "background-origin",
    "background-position",
    "background-repeat",
    "background-size",
    "border",
    "border-bottom",
    "border-bottom-color",
    "border-bottom-left-radius",
    "border-bottom-right-radius",
    "border-bottom-style",
    "border-bottom-width",
    "border-collapse",
    "border-color",
    "border-left",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "border-radius",
    "border-right",
    "border-right-color",
    "border-right-style",
    "border-right-width",
    "border-spacing",
    "border-style",
    "border-top",
    "border-top-color",
    "border-top-left-radius",
    "border-top-right-radius",
    "border-top-style",
    "border-top-width",
    "border-width",
    "box-shadow",
    "box-sizing",
    "caption-side",
    "clear",
    "color",
    "column-gap",
    "direction",
    "display",
    "empty-cells",
    "flex",
    "flex-basis",
    "flex-direction",
    "flex-flow",
    "flex-grow",
    "flex-shrink",
    "flex-wrap",
    "float",
    "font",
    "font-family",
    "font-size",
    "font-stretch",
    "font-style",
    "font-variant",
    "font-weight",
    "gap",
    "height",
    "hyphens",
    "justify-content",
    "justify-items",
    "justify-self",
    "letter-spacing",
    "line-height",
    "list-style-position",
    "list-style-type",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-height",
    "max-width",
    "min-height",
    "min-width",
    "object-fit",
    "object-position",
    "opacity",
    "overflow",
    "overflow-wrap",
    "overflow-x",
    "overflow-y",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "row-gap",
    "table-layout",
    "text-align",
    "text-decoration",
    "text-decoration-color",
    "text-decoration-line",
    "text-decoration-style",
    "text-indent",
    "text-overflow",
    "text-shadow",
    "text-transform",
    "vertical-align",
    "visibility",
    "white-space",
    "width",
    "word-break",
    "word-spacing",
];

const SAFE_FUNCTIONS: &[&str] = &[
    "calc", "clamp", "color", "hsl", "hsla", "hwb", "lab", "lch", "max", "min", "oklab", "oklch",
    "rgb", "rgba",
];

const SAFE_PSEUDO_CLASSES: &[&str] = &[
    "active",
    "checked",
    "disabled",
    "empty",
    "enabled",
    "first-child",
    "first-of-type",
    "focus",
    "focus-visible",
    "focus-within",
    "hover",
    "last-child",
    "last-of-type",
    "link",
    "only-child",
    "only-of-type",
    "root",
    "visited",
];

pub(crate) fn sanitize_declaration_list(css: &str) -> String {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    sanitize_declarations(&mut parser)
}

/// Whether an inline declaration gives an image a tracker-sized width or
/// height. Reuses the exact parser/allow-list that will feed WebKit, so CSS
/// escapes, comments and malformed declarations cannot create a second
/// interpretation of the style attribute.
pub(crate) fn declares_tiny_image_dimension(css: &str) -> bool {
    sanitize_declaration_list(css)
        .split_terminator(';')
        .filter_map(|declaration| declaration.split_once(':'))
        .any(|(name, value)| matches!(name, "width" | "height") && declares_tiny_dimension(value))
}

pub(crate) fn sanitize_style_blocks(html: &str) -> String {
    let mut output = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(start) = rest.find("<style>") {
        output.push_str(&rest[..start]);
        let content_start = start + "<style>".len();
        let Some(relative_end) = rest[content_start..].find("</style>") else {
            // `html` is ammonia's canonical serialization, so this should
            // not happen.  Fail closed if that invariant ever changes.
            return output;
        };
        let content_end = content_start + relative_end;
        let safe = sanitize_stylesheet(&rest[content_start..content_end]);
        if !safe.is_empty() {
            output.push_str("<style>");
            output.push_str(&safe);
            output.push_str("</style>");
        }
        rest = &rest[content_end + "</style>".len()..];
    }

    output.push_str(rest);
    output
}

fn sanitize_stylesheet(css: &str) -> String {
    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    sanitize_rules(&mut parser, 0)
}

fn sanitize_rules(input: &mut Parser<'_, '_>, depth: usize) -> String {
    let mut rule_parser = SafeRuleParser { depth };
    StyleSheetParser::new(input, &mut rule_parser)
        .filter_map(Result::ok)
        .collect()
}

fn sanitize_declarations(input: &mut Parser<'_, '_>) -> String {
    let mut declaration_parser = SafeDeclarationParser;
    RuleBodyParser::new(input, &mut declaration_parser)
        .filter_map(Result::ok)
        .collect()
}

struct SafeDeclarationParser;

impl<'i> DeclarationParser<'i> for SafeDeclarationParser {
    type Declaration = String;
    type Error = ();

    fn parse_value<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _declaration_start: &ParserState,
    ) -> Result<Self::Declaration, ParseError<'i, Self::Error>> {
        let name = name.to_ascii_lowercase();
        if !SAFE_PROPERTIES.contains(&name.as_str()) {
            return Err(
                input.new_error(BasicParseErrorKind::UnexpectedToken(Token::Ident(
                    name.into(),
                ))),
            );
        }

        let value = serialize_safe_value(input)?;
        if value.trim().is_empty() {
            return Err(input.new_error(BasicParseErrorKind::EndOfInput));
        }
        Ok(format!("{name}:{value};"))
    }
}

impl<'i> AtRuleParser<'i> for SafeDeclarationParser {
    type Prelude = ();
    type AtRule = String;
    type Error = ();
}

impl<'i> QualifiedRuleParser<'i> for SafeDeclarationParser {
    type Prelude = ();
    type QualifiedRule = String;
    type Error = ();
}

impl RuleBodyItemParser<'_, String, ()> for SafeDeclarationParser {
    fn parse_declarations(&self) -> bool {
        true
    }

    fn parse_qualified(&self) -> bool {
        false
    }
}

struct SafeRuleParser {
    depth: usize,
}

impl<'i> QualifiedRuleParser<'i> for SafeRuleParser {
    type Prelude = String;
    type QualifiedRule = String;
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, Self::Error>> {
        let selectors = input.parse_comma_separated(sanitize_selector)?;
        if selectors.is_empty() {
            return Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid));
        }
        Ok(selectors.join(","))
    }

    fn parse_block<'t>(
        &mut self,
        selectors: Self::Prelude,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::QualifiedRule, ParseError<'i, Self::Error>> {
        let declarations = sanitize_declarations(input);
        if declarations.is_empty() {
            return Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid));
        }
        Ok(format!("{selectors}{{{declarations}}}"))
    }
}

enum SafeAtRule {
    Media(String),
}

impl<'i> AtRuleParser<'i> for SafeRuleParser {
    type Prelude = SafeAtRule;
    type AtRule = String;
    type Error = ();

    fn parse_prelude<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::Prelude, ParseError<'i, Self::Error>> {
        if !name.eq_ignore_ascii_case("media") || self.depth >= MAX_RULE_NESTING {
            return Err(input.new_error(BasicParseErrorKind::AtRuleInvalid(name)));
        }
        let condition = serialize_safe_media(input)?;
        if condition.trim().is_empty() {
            return Err(input.new_error(BasicParseErrorKind::AtRuleInvalid(name)));
        }
        Ok(SafeAtRule::Media(condition))
    }

    fn parse_block<'t>(
        &mut self,
        prelude: Self::Prelude,
        _start: &ParserState,
        input: &mut Parser<'i, 't>,
    ) -> Result<Self::AtRule, ParseError<'i, Self::Error>> {
        let nested = sanitize_rules(input, self.depth + 1);
        if nested.is_empty() {
            return Err(input.new_error(BasicParseErrorKind::AtRuleBodyInvalid));
        }
        match prelude {
            SafeAtRule::Media(condition) => Ok(format!("@media {condition}{{{nested}}}")),
        }
    }
}

fn sanitize_selector<'i, 't>(input: &mut Parser<'i, 't>) -> Result<String, ParseError<'i, ()>> {
    let mut output = String::new();
    let mut previous = TokenSerializationType::Nothing;
    let mut expects_pseudo = false;

    loop {
        let token = match input.next_including_whitespace_and_comments().cloned() {
            Ok(token) => token,
            Err(error) if error.kind == BasicParseErrorKind::EndOfInput => break,
            Err(error) => return Err(error.into()),
        };
        match &token {
            Token::Ident(name) if expects_pseudo => {
                if !SAFE_PSEUDO_CLASSES
                    .iter()
                    .any(|allowed| name.eq_ignore_ascii_case(allowed))
                {
                    return Err(input.new_error(BasicParseErrorKind::UnexpectedToken(token)));
                }
                expects_pseudo = false;
            }
            Token::Ident(_) | Token::IDHash(_) | Token::WhiteSpace(_) => {
                if expects_pseudo && !matches!(token, Token::WhiteSpace(_)) {
                    return Err(input.new_error(BasicParseErrorKind::UnexpectedToken(token)));
                }
            }
            Token::Comment(_) => {
                push_separator(&mut output, &mut previous);
                continue;
            }
            Token::SquareBracketBlock if !expects_pseudo => {
                write_token(&token, &mut output, &mut previous);
                let nested = input.parse_nested_block(serialize_attribute_selector)?;
                output.push_str(&nested);
                output.push(']');
                previous = TokenSerializationType::Other;
                continue;
            }
            Token::Colon if !expects_pseudo => expects_pseudo = true,
            Token::Delim('.' | '*' | '>' | '+' | '~') if !expects_pseudo => {}
            _ => return Err(input.new_error(BasicParseErrorKind::UnexpectedToken(token))),
        }
        write_token(&token, &mut output, &mut previous);
    }

    if expects_pseudo || output.trim().is_empty() {
        return Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid));
    }
    Ok(scope_selector(output.trim()))
}

fn serialize_attribute_selector<'i, 't>(
    input: &mut Parser<'i, 't>,
) -> Result<String, ParseError<'i, ()>> {
    let mut output = String::new();
    let mut previous = TokenSerializationType::Nothing;

    loop {
        let token = match input.next_including_whitespace_and_comments().cloned() {
            Ok(token) => token,
            Err(error) if error.kind == BasicParseErrorKind::EndOfInput => break,
            Err(error) => return Err(error.into()),
        };
        match &token {
            Token::Ident(_)
            | Token::QuotedString(_)
            | Token::WhiteSpace(_)
            | Token::Delim('=')
            | Token::IncludeMatch
            | Token::DashMatch
            | Token::PrefixMatch
            | Token::SuffixMatch
            | Token::SubstringMatch => {}
            Token::Comment(_) => {
                push_separator(&mut output, &mut previous);
                continue;
            }
            _ => return Err(input.new_error(BasicParseErrorKind::UnexpectedToken(token))),
        }
        write_token(&token, &mut output, &mut previous);
    }

    if output.trim().is_empty() {
        return Err(input.new_error(BasicParseErrorKind::QualifiedRuleInvalid));
    }
    Ok(output)
}

fn scope_selector(selector: &str) -> String {
    let mut rest = selector.trim();
    let mut replaced_document_root = false;
    if let Some(after_html) = strip_leading_element(rest, "html") {
        rest = after_html.trim_start();
        replaced_document_root = true;
    }
    if let Some(after_body) = strip_leading_element(rest, "body") {
        rest = after_body;
        replaced_document_root = true;
    }
    if rest.eq_ignore_ascii_case(":root") {
        rest = "";
        replaced_document_root = true;
    }

    if rest.is_empty() {
        ".fm-message".to_string()
    } else if replaced_document_root && rest.starts_with(['.', '#', ':', '>', '+', '~']) {
        format!(".fm-message{rest}")
    } else {
        format!(".fm-message {rest}")
    }
}

fn strip_leading_element<'a>(selector: &'a str, element: &str) -> Option<&'a str> {
    let prefix = selector.get(..element.len())?;
    if !prefix.eq_ignore_ascii_case(element) {
        return None;
    }
    let rest = &selector[element.len()..];
    if rest.is_empty()
        || rest
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_whitespace() || ".#:>+~".contains(ch))
    {
        Some(rest)
    } else {
        None
    }
}

fn serialize_safe_value<'i, 't>(input: &mut Parser<'i, 't>) -> Result<String, ParseError<'i, ()>> {
    serialize_component_values(input, ComponentPolicy::Value)
}

fn serialize_safe_media<'i, 't>(input: &mut Parser<'i, 't>) -> Result<String, ParseError<'i, ()>> {
    serialize_component_values(input, ComponentPolicy::Media)
}

#[derive(Clone, Copy)]
enum ComponentPolicy {
    Value,
    Media,
}

fn serialize_component_values<'i, 't>(
    input: &mut Parser<'i, 't>,
    policy: ComponentPolicy,
) -> Result<String, ParseError<'i, ()>> {
    let mut output = String::new();
    let mut previous = TokenSerializationType::Nothing;

    loop {
        let token = match input.next_including_whitespace_and_comments().cloned() {
            Ok(token) => token,
            Err(error) if error.kind == BasicParseErrorKind::EndOfInput => break,
            Err(error) => return Err(error.into()),
        };

        match &token {
            Token::Comment(_) => {
                push_separator(&mut output, &mut previous);
                continue;
            }
            Token::UnquotedUrl(_)
            | Token::BadUrl(_)
            | Token::BadString(_)
            | Token::AtKeyword(_)
            | Token::CurlyBracketBlock
            | Token::SquareBracketBlock
            | Token::CloseParenthesis
            | Token::CloseSquareBracket
            | Token::CloseCurlyBracket
            | Token::CDO
            | Token::CDC => {
                return Err(input.new_error(BasicParseErrorKind::UnexpectedToken(token)));
            }
            Token::Function(name) => {
                if !matches!(policy, ComponentPolicy::Value)
                    || !SAFE_FUNCTIONS
                        .iter()
                        .any(|allowed| name.eq_ignore_ascii_case(allowed))
                {
                    return Err(input.new_error(BasicParseErrorKind::UnexpectedToken(token)));
                }
                write_token(&token, &mut output, &mut previous);
                let nested = input.parse_nested_block(|nested| {
                    serialize_component_values(nested, ComponentPolicy::Value)
                })?;
                output.push_str(&nested);
                output.push(')');
                previous = TokenSerializationType::Other;
                continue;
            }
            Token::ParenthesisBlock => {
                if !matches!(policy, ComponentPolicy::Media) {
                    return Err(input.new_error(BasicParseErrorKind::UnexpectedToken(token)));
                }
                write_token(&token, &mut output, &mut previous);
                let nested = input.parse_nested_block(|nested| {
                    serialize_component_values(nested, ComponentPolicy::Media)
                })?;
                output.push_str(&nested);
                output.push(')');
                previous = TokenSerializationType::Other;
                continue;
            }
            _ => {}
        }
        write_token(&token, &mut output, &mut previous);
    }
    Ok(output.trim().to_string())
}

fn push_separator(output: &mut String, previous: &mut TokenSerializationType) {
    if !output.chars().last().is_some_and(char::is_whitespace) {
        output.push(' ');
    }
    *previous = TokenSerializationType::WhiteSpace;
}

fn write_token(token: &Token<'_>, output: &mut String, previous: &mut TokenSerializationType) {
    let current = token.serialization_type();
    if previous.needs_separator_when_before(current) {
        output.push(' ');
    }
    // `String`'s `fmt::Write` implementation never returns `fmt::Error`.
    let _ = token.to_css(output);
    *previous = current;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_layout_survives_but_network_and_overlay_properties_do_not() {
        let safe = sanitize_declaration_list(concat!(
            "width:600px;padding:24px;background:#fff;box-shadow:0 1px 3px #ddd;",
            "background-image:url(https://tracker.example/p.gif);",
            "position:fixed;inset:0;z-index:9999;color:rgb(1 2 3);"
        ));
        for kept in [
            "width:600px",
            "padding:24px",
            "background:#fff",
            "box-shadow:0 1px 3px #ddd",
            "color:rgb(",
        ] {
            assert!(safe.contains(kept), "{kept} missing from {safe}");
        }
        for dropped in ["url", "tracker", "position", "inset", "z-index"] {
            assert!(!safe.contains(dropped), "{dropped} survived in {safe}");
        }
    }

    #[test]
    fn stylesheet_keeps_scoped_layout_and_safe_media_only() {
        let safe = sanitize_stylesheet(concat!(
            "body{margin:0;background-color:#fff}",
            ".card td{padding:12px;width:50%}",
            "@media screen and (max-width:600px){.card{width:100%}}",
            "@import url(https://evil.example/x.css);",
            "@font-face{font-family:x;src:url(https://evil.example/x.woff)}"
        ));
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
    fn comments_cannot_reassemble_a_url_function() {
        let safe = sanitize_declaration_list("color:red;background-color:u/**/rl(x)");
        assert!(safe.contains("color:red"), "{safe}");
        assert!(!safe.to_ascii_lowercase().contains("url("), "{safe}");
    }

    #[test]
    fn escaped_network_functions_are_tokens_and_still_rejected() {
        let safe = sanitize_declaration_list(concat!(
            r#"background:u\72l("https://evil.example/p.gif");"#,
            "color:#123456;behavior:expression(alert(1));"
        ));
        assert_eq!(safe, "color:#123456;");
    }

    #[test]
    fn tracker_dimensions_are_read_through_the_same_css_parser() {
        for style in [
            "width:1px",
            "HEIGHT: 0 !important",
            "color:red;width:/**/1PX",
        ] {
            assert!(declares_tiny_image_dimension(style), "{style}");
        }
        for style in [
            "width:10px",
            "height:1%",
            "inline-size:1px",
            "width:calc(1px)",
        ] {
            assert!(!declares_tiny_image_dimension(style), "{style}");
        }
    }

    #[test]
    fn selectors_keep_safe_attributes_but_drop_generated_content_and_has() {
        let safe = sanitize_stylesheet(concat!(
            ".ok:hover{color:blue}",
            r#"a[href^="https"]{color:#2463eb}"#,
            ".bad::before{content:'spoof'}",
            ".also:has(img){color:red}"
        ));
        assert!(
            safe.contains(".fm-message .ok:hover{color:blue;}"),
            "{safe}"
        );
        assert!(
            safe.contains(r#".fm-message a[href^="https"]{color:#2463eb;}"#),
            "{safe}"
        );
        for dropped in ["before", "content", ":has"] {
            assert!(!safe.contains(dropped), "{dropped} survived in {safe}");
        }
    }
}
