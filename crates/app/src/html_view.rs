//! T-030 (second half): turn a cached RFC 822 body into something the
//! reading pane can show, and isolate HTML inside a WebKitGTK widget.
//!
//! The sanitizer itself lives in `feathermail-html`. This module is the
//! only place `crates/app` is allowed to hand HTML to a renderer: it
//! runs [`feathermail_html::sanitize`] off the GTK thread, wraps the
//! result in a document with a Content-Security-Policy, and loads that
//! into a WebView whose JavaScript, popups, file access, and permission
//! prompts are all off (D3, D44).
//!
//! Pure decisions (`prepare_body`, [`link_decision`], [`response_is_allowed`],
//! the CSP string) are free functions so the security forks can be tested
//! without a display. Building the widget itself requires GTK and is not
//! covered by `cargo test`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use feathermail_html::{
    sanitize_message_html, BodyContent, SanitizeOptions, SanitizeReport, DEFAULT_MAX_INPUT_BYTES,
};
use gtk::gdk;
use relm4::gtk;
use webkit6::prelude::*;

use crate::msg::Msg;

/// How much of a *parsed* plain-text body the label shows. Same budget
/// T-080 used: large enough for a typical message, small enough that a
/// multi-megabyte body cannot make the GTK thread build a giant string
/// (D11: the label is rebuilt from this already-truncated text inside
/// `#[watch]`, never from the raw bytes).
pub const RAW_BODY_PREVIEW_BYTES: usize = 64 * 1024;

/// Options for [`prepare_body`]. Mirrors the Privacy toggles the shell
/// already has (`block_remote`, `block_pixels`, `prefer_plain`) plus the
/// per-message "Show images" override.
///
/// T-141: `prefer_plain` defaults **off** -- a `multipart/alternative`
/// shows its sanitized HTML half unless the reader turns the setting on.
/// It defaulted on until then (Fork D-plain-default), and the owner's
/// report is what that cost: a Jira notification read "Логотип Jira
/// [https://...png]" because the plain half is where an image becomes its
/// own alt text and a bracketed URL. Remote images stay blocked either
/// way -- that is `block_remote`, a different toggle with its own bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrepareOpts {
    pub allow_remote_images: bool,
    pub block_tracking_pixels: bool,
    pub prefer_plain: bool,
}

impl Default for PrepareOpts {
    fn default() -> Self {
        Self {
            allow_remote_images: false,
            block_tracking_pixels: true,
            prefer_plain: false,
        }
    }
}

/// Map the Privacy toggles plus the per-message "Show images" override
/// onto [`PrepareOpts`]. `block_remote` / `block_pixels` are the
/// settings as stored (`true` = the protection is on). `prefer_plain`
/// is forwarded as-is (default off, T-141). `allow_this_message` is already the
/// OR of the session click and a remembered sender domain (T-117).
pub fn prepare_opts(
    block_remote: bool,
    block_pixels: bool,
    allow_this_message: bool,
    prefer_plain: bool,
) -> PrepareOpts {
    PrepareOpts {
        allow_remote_images: allow_this_message || !block_remote,
        block_tracking_pixels: block_pixels,
        prefer_plain,
    }
}

/// T-117: Show images is remembered by the sender's domain, so the next
/// letter from the same host does not wait for another click.
pub fn allow_images_for_sender(
    allow_this_message: bool,
    sender_domain: Option<&str>,
    allowed_domains: &std::collections::HashSet<String>,
) -> bool {
    allow_this_message || sender_domain.is_some_and(|domain| allowed_domains.contains(domain))
}

/// What the reading pane should show once parse + sanitize have run.
///
/// D14: [`Debug`] never prints message content — only variant, lengths,
/// and the sanitizer's counts.
#[derive(Clone, PartialEq, Eq)]
pub enum PreparedBody {
    Empty {
        attachments: usize,
    },
    Plain {
        text: String,
        attachments: usize,
    },
    Html {
        sanitized: String,
        report: SanitizeReport,
        attachments: usize,
        allow_remote_images: bool,
    },
    Undecodable {
        message: String,
        attachments: usize,
    },
    Oversized {
        attachments: usize,
    },
}

impl std::fmt::Debug for PreparedBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty { attachments } => f
                .debug_struct("Empty")
                .field("attachments", attachments)
                .finish(),
            Self::Plain { text, attachments } => f
                .debug_struct("Plain")
                .field("len", &text.len())
                .field("attachments", attachments)
                .finish(),
            Self::Html {
                sanitized,
                report,
                attachments,
                allow_remote_images,
            } => f
                .debug_struct("Html")
                .field("len", &sanitized.len())
                .field("report", report)
                .field("attachments", attachments)
                .field("allow_remote_images", allow_remote_images)
                .finish(),
            Self::Undecodable {
                message,
                attachments,
            } => f
                .debug_struct("Undecodable")
                .field("message", message)
                .field("attachments", attachments)
                .finish(),
            Self::Oversized { attachments } => f
                .debug_struct("Oversized")
                .field("attachments", attachments)
                .finish(),
        }
    }
}

impl PreparedBody {
    pub fn is_html(&self) -> bool {
        matches!(self, Self::Html { .. })
    }

    /// Text for the plain-text label. Empty when the WebView should be
    /// showing the body instead.
    pub fn label_text(&self) -> String {
        let mut out = match self {
            Self::Empty { .. } => "(this message has no content)".to_string(),
            Self::Plain { text, .. } => text.clone(),
            Self::Html { .. } => String::new(),
            Self::Undecodable { message, .. } => message.clone(),
            Self::Oversized { .. } => "This message is too large to show.".to_string(),
        };
        if !self.is_html() {
            if let Some(note) = attachment_note(self.attachments()) {
                out.push_str(&note);
            }
        }
        out
    }

    pub fn attachments(&self) -> usize {
        match self {
            Self::Empty { attachments }
            | Self::Plain { attachments, .. }
            | Self::Html { attachments, .. }
            | Self::Undecodable { attachments, .. }
            | Self::Oversized { attachments } => *attachments,
        }
    }

    /// Banner above an HTML body when remote images were stripped and
    /// the user has not opted this message in. `None` when there is
    /// nothing to offer.
    pub fn images_banner(&self) -> Option<String> {
        match self {
            Self::Html {
                report,
                allow_remote_images,
                ..
            } if !*allow_remote_images && report.blocked_remote_images > 0 => {
                let n = report.blocked_remote_images;
                Some(if n == 1 {
                    "1 remote image was blocked.".to_string()
                } else {
                    format!("{n} remote images were blocked.")
                })
            }
            _ => None,
        }
    }

    /// Line under the WebView: attachment count and (when relevant) that
    /// cid: inline images cannot be shown yet.
    pub fn html_footer(&self) -> Option<String> {
        let Self::Html {
            report,
            attachments,
            ..
        } = self
        else {
            return None;
        };
        // T-101: no attachment count here. The owner's words: "this message
        // has n attachments -- убрать снизу у кнопки реплая". The line said
        // nothing the pane was not already showing -- every attachment is a
        // row of its own above the body, with its name, its size and its
        // Open/Save buttons -- and it sat between the letter and the Reply
        // button, which is the one place in the pane where a sentence reads
        // as part of the message.
        let mut parts: Vec<String> = Vec::new();
        let _ = attachments;
        if report.blocked_cid_images > 0 {
            parts.push("Inline images from attachments can’t be shown yet.".to_string());
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    }
}

/// Parse + sanitize off the GTK thread. `opts.prefer_plain` selects the
/// `text/plain` part of a `multipart/alternative` (Privacy "Prefer
/// plain text", default on). HTML-only messages go through [`sanitize`]
/// and become [`PreparedBody::Html`] regardless of the flag.
pub fn prepare_body(bytes: &[u8], opts: &PrepareOpts) -> PreparedBody {
    let parsed = feathermail_html::parse_message(bytes, opts.prefer_plain);
    let attachments = parsed.attachments.len();
    match parsed.body {
        BodyContent::Empty => PreparedBody::Empty { attachments },
        BodyContent::Plain(text) if text.is_empty() => PreparedBody::Empty { attachments },
        BodyContent::Plain(text) => {
            let (shown, truncated) = truncate_text_preview(&text, RAW_BODY_PREVIEW_BYTES);
            let mut text = shown.to_string();
            if truncated {
                text.push_str("\n\n[showing only the first part of a longer message]");
            }
            PreparedBody::Plain { text, attachments }
        }
        BodyContent::Undecodable(err) => PreparedBody::Undecodable {
            message: err.to_string(),
            attachments,
        },
        BodyContent::Html(raw) => {
            let (sanitized, report) = sanitize_message_html(
                &raw,
                bytes,
                &SanitizeOptions {
                    allow_remote_images: opts.allow_remote_images,
                    block_tracking_pixels: opts.block_tracking_pixels,
                    max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
                },
            );
            if report.oversized_input {
                PreparedBody::Oversized { attachments }
            } else {
                PreparedBody::Html {
                    sanitized: sanitized.as_sanitized_str().to_string(),
                    report,
                    attachments,
                    allow_remote_images: opts.allow_remote_images,
                }
            }
        }
    }
}

/// D11: parse + sanitize never run on the caller's thread. Same shape
/// as `spawn_body_lookup`: the production path is [`spawn_prepare_body`],
/// and tests inject a blocking `prepare` so a mutex held on the caller
/// proves the work moved off-thread.
pub fn spawn_prepare_body(
    bytes: Vec<u8>,
    opts: PrepareOpts,
    gen: u64,
    sink: impl FnOnce(Msg) + Send + 'static,
) {
    spawn_prepare_body_with(bytes, opts, gen, prepare_body, sink);
}

pub(crate) fn spawn_prepare_body_with<F>(
    bytes: Vec<u8>,
    opts: PrepareOpts,
    gen: u64,
    prepare: F,
    sink: impl FnOnce(Msg) + Send + 'static,
) where
    F: FnOnce(&[u8], &PrepareOpts) -> PreparedBody + Send + 'static,
{
    std::thread::spawn(move || {
        let prepared = prepare(&bytes, &opts);
        sink(Msg::BodyPrepared { gen, prepared });
    });
}

fn attachment_note(n: usize) -> Option<String> {
    match n {
        0 => None,
        1 => Some("\n\n[this message has 1 attachment]".to_string()),
        n => Some(format!("\n\n[this message has {n} attachments]")),
    }
}

pub(crate) fn truncate_text_preview(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// DESIGN.md tokens for the HTML document — WebKit does not inherit GTK
/// CSS, so the reading pane has to paint with the same hex values the
/// shell already uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HtmlPalette {
    pub ink: &'static str,
    pub paper: &'static str,
    pub link: &'static str,
}

impl HtmlPalette {
    pub fn for_dark(dark: bool) -> Self {
        if dark {
            Self {
                ink: "#f3f4f6",
                paper: "#181b1f",
                link: "#60a5fa",
            }
        } else {
            Self {
                ink: "#0b0c0e",
                paper: "#ffffff",
                link: "#1a58f4",
            }
        }
    }
}

/// CSP used both as the WebView's construct-only default (the maximum
/// this widget will ever allow) and as a per-document meta tag that
/// tightens `img-src` when remote images are still blocked.
pub fn content_security_policy(allow_remote_images: bool) -> &'static str {
    if allow_remote_images {
        "default-src 'none'; img-src data: http: https:; style-src 'unsafe-inline'; \
         script-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; \
         base-uri 'none'; form-action 'none'; media-src 'none'"
    } else {
        "default-src 'none'; img-src data:; style-src 'unsafe-inline'; \
         script-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; \
         base-uri 'none'; form-action 'none'; media-src 'none'"
    }
}

/// Wrap already-sanitized HTML in a document the WebView can load.
/// The fragment is inserted as HTML, not escaped: it has already been
/// through [`sanitize`]. Palette values are our own hex literals.
pub fn wrap_sanitized_document(
    sanitized: &str,
    allow_remote_images: bool,
    palette: HtmlPalette,
) -> String {
    let csp = content_security_policy(allow_remote_images);
    format!(
        "<!DOCTYPE html>\
         <html><head>\
         <meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">\
         <style>\
         html,body{{margin:0;padding:0;background:{paper};color:{ink};\
         font-family:Inter,\"Adwaita Sans\",Cantarell,sans-serif;\
         font-size:16px;font-weight:400;line-height:1.7;}}\
         a{{color:{link};}}\
         img{{max-width:100%;height:auto;}}\
         </style>\
         </head><body class=\"fm-message\">{sanitized}</body></html>",
        paper = palette.paper,
        ink = palette.ink,
        link = palette.link,
        sanitized = sanitized,
        csp = csp,
    )
}

/// What to do with a URI the user clicked inside the HTML body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkDecision {
    /// `javascript:`, `data:`, `file:`, `vbscript:`, `blob:`, `about:` —
    /// never leave the app, even if the user would confirm.
    Refuse,
    /// `http`/`https` with the confirm-links setting off.
    Open,
    /// Everything else that is safe to hand to the desktop handler:
    /// `http`/`https` with the setting on, and `mailto:` (D44: non-http
    /// always confirms).
    Confirm,
}

/// Classify `uri` for the external-browser path. The WebView itself
/// never navigates to these — [`IsolatedHtmlView`] intercepts the click
/// and this function decides whether to launch, ask, or refuse.
pub fn link_decision(uri: &str, confirm_http: bool) -> LinkDecision {
    let scheme = uri
        .split_once(':')
        .map(|(s, _)| s)
        .unwrap_or("")
        .to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" => {
            if confirm_http {
                LinkDecision::Confirm
            } else {
                LinkDecision::Open
            }
        }
        "mailto" => LinkDecision::Confirm,
        _ => LinkDecision::Refuse,
    }
}

/// Human text for [`LinkDecision::Refuse`]. Names the scheme, never the
/// rest of the URI — a `file:///etc/passwd` click must not put that
/// path on screen (D14).
pub fn refuse_link_toast(uri: &str) -> &'static str {
    let scheme = uri
        .split_once(':')
        .map(|(s, _)| s)
        .unwrap_or("")
        .to_ascii_lowercase();
    match scheme.as_str() {
        "file" => "Feather Mail refused to open a local-file link.",
        "javascript" | "data" | "vbscript" | "blob" => "Feather Mail refused to run that link.",
        _ => "Feather Mail refused to open that link.",
    }
}

/// T-130: the schemes a plain-text body may turn into a live link.
/// Exactly the ones [`link_decision`] does not refuse — a body cannot
/// invent a clickable `javascript:` or `file:` by writing one out in
/// text, because nothing here recognises it in the first place.
const LINK_SCHEMES: [&str; 3] = ["https://", "http://", "mailto:"];

/// T-130: how far a link that starts at the beginning of `rest` runs.
/// `None` when `rest` does not start with a known scheme, or when
/// nothing but the scheme is left after the trailing punctuation is
/// trimmed.
///
/// The end is the first whitespace, control character or quoting
/// character; then sentence punctuation is trimmed off the tail
/// (`Открой https://example.com.` — the full stop is the sentence's,
/// not the URL's), and a closing bracket is trimmed only when the URL
/// does not open one itself (Wikipedia's `...(disambiguation)` stays
/// whole).
fn link_span(rest: &str) -> Option<usize> {
    let scheme = LINK_SCHEMES.iter().find(|scheme| {
        rest.len() >= scheme.len()
            && rest.as_bytes()[..scheme.len()].eq_ignore_ascii_case(scheme.as_bytes())
    })?;
    let mut end = rest.len();
    for (offset, ch) in rest.char_indices() {
        if ch.is_whitespace() || ch.is_control() || matches!(ch, '<' | '>' | '"' | '\'' | '«' | '»')
        {
            end = offset;
            break;
        }
    }
    let mut span = &rest[..end];
    loop {
        let last = span.chars().next_back()?;
        let trimmed = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '…' => &span[..span.len() - last.len_utf8()],
            ')' | ']' | '}' => {
                let open = match last {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if span.matches(open).count() >= span.matches(last).count() {
                    break;
                }
                &span[..span.len() - last.len_utf8()]
            }
            _ => break,
        };
        span = trimmed;
    }
    if span.len() <= scheme.len() {
        return None;
    }
    Some(span.len())
}

/// T-130: Pango markup for a plain-text body. Everything is escaped
/// first, so no `<b>` a correspondent typed can reach the label as
/// markup; the only tags in the result are the `<a href>` this function
/// writes around a URL it recognised itself.
///
/// Owner, on the live mailbox: «Ссылки в теле писем показываются не
/// ссылками, а корявым текстом». `prefer_plain` is on by default, so
/// most mail is drawn by the reading pane's label — and a label without
/// markup has no links at all.
///
/// The click still goes through the one door: the shell hands the URI
/// to [`link_decision`] exactly as it does for an anchor inside
/// sanitized HTML (D44).
pub fn plain_body_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut cursor = 0usize;
    let mut idx = 0usize;
    let mut prev: Option<char> = None;
    while idx < text.len() {
        let rest = &text[idx..];
        let ch = match rest.chars().next() {
            Some(ch) => ch,
            None => break,
        };
        // A scheme glued to the end of a word (`see:http://…` is fine,
        // `xhttp://…` is not) is not a link start.
        let at_boundary = prev.is_none_or(|p| !p.is_alphanumeric());
        if at_boundary {
            if let Some(len) = link_span(rest) {
                let uri = &rest[..len];
                if link_decision(uri, true) != LinkDecision::Refuse {
                    out.push_str(&escape_markup(&text[cursor..idx]));
                    out.push_str("<a href=\"");
                    out.push_str(&escape_markup(uri));
                    out.push_str("\">");
                    out.push_str(&escape_markup(uri));
                    out.push_str("</a>");
                    idx += len;
                    cursor = idx;
                    prev = uri.chars().next_back();
                    continue;
                }
            }
        }
        idx += ch.len_utf8();
        prev = Some(ch);
    }
    out.push_str(&escape_markup(&text[cursor..]));
    out
}

/// `&`, `<`, `>`, `\'` and `"` — the five characters Pango would read as
/// markup. GLib's own escaper, so the label and the parser agree.
fn escape_markup(text: &str) -> String {
    gtk::glib::markup_escape_text(text).to_string()
}

pub fn is_internal_load(uri: &str) -> bool {
    uri.is_empty() || uri == "about:blank" || uri.starts_with("about:blank")
}

/// Subresource policy for `decide-policy` of type Response. When remote
/// images are blocked the WebView is not allowed any network at all —
/// sanitizer + document CSP are the other two layers; this is the one
/// that still fires if both of those miss.
pub fn response_is_allowed(uri: &str, allow_remote: bool) -> bool {
    if is_internal_load(uri) {
        return true;
    }
    if uri.starts_with("data:image/") {
        return true;
    }
    allow_remote && (uri.starts_with("http://") || uri.starts_with("https://"))
}

/// Isolated WebView plus the flag [`response_is_allowed`] reads. The
/// shell creates it only for an HTML body and drops it again when that
/// preview goes away, never from a `#[watch]` block.
pub struct IsolatedHtmlView {
    pub webview: webkit6::WebView,
    allow_remote: Arc<AtomicBool>,
}

impl IsolatedHtmlView {
    /// Construct on the GTK thread after GTK is initialized. `on_link`
    /// receives every URI the page tried to navigate to other than the
    /// `load_html` of `about:blank` itself.
    pub fn new(on_link: impl Fn(String) + 'static) -> Self {
        let settings = webkit6::Settings::new();
        settings.set_enable_javascript(false);
        settings.set_enable_javascript_markup(false);
        settings.set_javascript_can_open_windows_automatically(false);
        settings.set_javascript_can_access_clipboard(false);
        settings.set_allow_file_access_from_file_urls(false);
        settings.set_allow_universal_access_from_file_urls(false);
        settings.set_disable_web_security(false);
        settings.set_enable_html5_database(false);
        settings.set_enable_html5_local_storage(false);
        settings.set_enable_offline_web_application_cache(false);
        settings.set_enable_page_cache(false);
        // Hyperlink auditing is deliberately not set here. WebKitGTK 2.52
        // deprecated the setter to a no-op that only prints a warning on
        // every view we build, and the guarantee does not depend on it:
        // `feathermail_html::sanitize` allows exactly `href`/`hreflang` on
        // `<a>`, so no `ping` attribute survives into the document, and the
        // per-message CSP is `connect-src 'none'` on top of that.
        settings.set_enable_media(false);
        settings.set_enable_media_stream(false);
        settings.set_enable_mediasource(false);
        settings.set_enable_webgl(false);
        settings.set_enable_developer_extras(false);
        settings.set_enable_write_console_messages_to_stdout(false);

        let allow_remote = Arc::new(AtomicBool::new(false));
        let webview = webkit6::WebView::builder()
            .network_session(&webkit6::NetworkSession::new_ephemeral())
            .settings(&settings)
            .default_content_security_policy(content_security_policy(true))
            .editable(false)
            .build();
        webview.set_hexpand(true);
        webview.set_vexpand(true);
        webview.set_background_color(&paper_rgba("#ffffff"));

        webview.connect_create(|_, _| None);

        // Temporary instrumentation (FEATHERMAIL_BODY_LOG): how long the
        // WebKit side actually takes between `load_html` and a painted page.
        webview.connect_load_changed(|_, event| {
            crate::bodylog::line(format_args!("webview load_changed {event:?}"));
        });

        webview.connect_permission_request(|_, request| {
            request.deny();
            true
        });

        let links = on_link;
        let allow_for_policy = Arc::clone(&allow_remote);
        webview.connect_decide_policy(move |_, decision, decision_type| match decision_type {
            webkit6::PolicyDecisionType::NewWindowAction => {
                decision.ignore();
                true
            }
            webkit6::PolicyDecisionType::NavigationAction => {
                let Some(nav) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() else {
                    decision.ignore();
                    return true;
                };
                let uri = nav
                    .navigation_action()
                    .and_then(|a| a.request())
                    .and_then(|r| r.uri())
                    .map(|u| u.to_string())
                    .unwrap_or_default();
                if is_internal_load(&uri) {
                    return false;
                }
                decision.ignore();
                links(uri);
                true
            }
            webkit6::PolicyDecisionType::Response => {
                let uri = decision
                    .downcast_ref::<webkit6::ResponsePolicyDecision>()
                    .and_then(|r| r.request())
                    .and_then(|r| r.uri())
                    .map(|u| u.to_string())
                    .unwrap_or_default();
                let allow = allow_for_policy.load(Ordering::Relaxed);
                if response_is_allowed(&uri, allow) {
                    false
                } else {
                    decision.ignore();
                    true
                }
            }
            _ => {
                decision.ignore();
                true
            }
        });

        Self {
            webview,
            allow_remote,
        }
    }

    pub fn set_allow_remote(&self, allow: bool) {
        self.allow_remote.store(allow, Ordering::Relaxed);
    }

    pub fn load(&self, html: &str, palette: HtmlPalette) {
        self.webview
            .set_background_color(&paper_rgba(palette.paper));
        self.webview.load_html(html, Some("about:blank"));
    }
}

/// DESIGN.md paper-pane as a GDK color. The two literals are the same
/// hex values `HtmlPalette` already pins; anything else falls back to
/// white rather than guessing.
fn paper_rgba(paper: &str) -> gdk::RGBA {
    match paper {
        "#181b1f" => gdk::RGBA::new(24.0 / 255.0, 27.0 / 255.0, 31.0 / 255.0, 1.0),
        _ => gdk::RGBA::new(1.0, 1.0, 1.0, 1.0),
    }
}

#[cfg(test)]
mod tests {
    /// T-130. The owner, on the live mailbox: «Ссылки в теле писем
    /// показываются не ссылками, а корявым текстом». A plain-text body
    /// is drawn by a label, and a label without markup has no links --
    /// so the URL has to come out of `plain_body_markup` as an anchor
    /// whose href and text are both the URL itself.
    #[test]
    fn a_url_in_a_plain_body_becomes_a_link() {
        let markup = plain_body_markup("Смотри https://example.com/order?id=7 и жди.");
        assert!(
            markup.contains(
                "<a href=\"https://example.com/order?id=7\">https://example.com/order?id=7</a>"
            ),
            "the URL must be an anchor, got {markup}"
        );
        assert!(
            markup.starts_with("Смотри ") && markup.ends_with(" и жди."),
            "the text around it must survive, got {markup}"
        );
    }

    /// Both other schemes the shell is allowed to hand to the desktop.
    #[test]
    fn http_and_mailto_are_links_too() {
        let http = plain_body_markup("http://example.org/a");
        assert!(
            http.contains("<a href=\"http://example.org/a\">"),
            "plain http must link, got {http}"
        );
        let mail = plain_body_markup("пиши mailto:sam@example.com");
        assert!(
            mail.contains("<a href=\"mailto:sam@example.com\">"),
            "mailto must link, got {mail}"
        );
    }

    /// The linkifier recognises exactly the schemes [`link_decision`]
    /// does not refuse. A correspondent who writes `javascript:` or
    /// `file:///etc/passwd` into a plain body must not get a clickable
    /// one -- the refusal toast is the second line of defence, this is
    /// the first (D44).
    #[test]
    fn a_refused_scheme_never_becomes_a_link() {
        for hostile in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,<b>x</b>",
            "vbscript:msgbox",
        ] {
            let markup = plain_body_markup(hostile);
            assert!(
                !markup.contains("<a "),
                "{hostile} must stay text, got {markup}"
            );
            assert_eq!(
                link_decision(hostile, true),
                LinkDecision::Refuse,
                "{hostile} is refused by the one door, so it must not be linkified either"
            );
        }
    }

    /// Everything that is not a link this function wrote is escaped:
    /// mail that contains `<b>` or a bare `&` must reach the label as
    /// those characters, not as markup Pango parses (and not as a parse
    /// error that blanks the pane).
    #[test]
    fn the_body_text_is_escaped_before_it_reaches_pango() {
        let markup = plain_body_markup("<b>жирно</b> & <i>косо</i>");
        assert_eq!(
            markup,
            "&lt;b&gt;жирно&lt;/b&gt; &amp; &lt;i&gt;косо&lt;/i&gt;"
        );
        let inside = plain_body_markup("https://example.com/?a=1&b=2");
        assert!(
            inside.contains("href=\"https://example.com/?a=1&amp;b=2\""),
            "the ampersand inside the href is escaped too, got {inside}"
        );
    }

    /// Sentence punctuation belongs to the sentence, a bracket the URL
    /// opened belongs to the URL.
    #[test]
    fn the_link_ends_where_the_url_ends() {
        let dot = plain_body_markup("Открой https://example.com/page.");
        assert!(
            dot.contains("<a href=\"https://example.com/page\">https://example.com/page</a>."),
            "the full stop stays outside, got {dot}"
        );
        let wrapped = plain_body_markup("(см. https://example.com/a)");
        assert!(
            wrapped.contains("<a href=\"https://example.com/a\">https://example.com/a</a>)"),
            "the closing bracket the URL did not open stays outside, got {wrapped}"
        );
        let balanced = plain_body_markup("https://ru.wikipedia.org/wiki/Ключ_(значения)");
        assert!(
            balanced.contains("wiki/Ключ_(значения)</a>"),
            "a bracket the URL opened stays inside, got {balanced}"
        );
    }

    /// A scheme glued to the end of a word is not a link start, and a
    /// naked scheme with nothing after it is not a link at all.
    #[test]
    fn only_a_real_url_is_linkified() {
        for quiet in ["xhttps://example.com", "https://", "mailto:"] {
            let markup = plain_body_markup(quiet);
            assert!(
                !markup.contains("<a "),
                "{quiet} must stay text, got {markup}"
            );
        }
    }

    /// T-101: the line under the letter no longer counts attachments.
    ///
    /// The owner: "this message has n attachments убрать снизу у кнопки
    /// реплая". Every attachment is already a row of its own above the body,
    /// with its name, its size and its Open/Save buttons; the sentence
    /// repeated that between the letter and the Reply button, which is the
    /// one place in the pane where a line of text reads as part of the
    /// message. What stays is the thing nothing else says: inline `cid:`
    /// images cannot be shown yet.
    #[test]
    fn the_footer_no_longer_counts_attachments() {
        let quiet = PreparedBody::Html {
            sanitized: String::new(),
            report: SanitizeReport::default(),
            attachments: 3,
            allow_remote_images: false,
        };
        assert_eq!(quiet.html_footer(), None);
        let inline = PreparedBody::Html {
            sanitized: String::new(),
            report: SanitizeReport {
                blocked_cid_images: 2,
                ..SanitizeReport::default()
            },
            attachments: 3,
            allow_remote_images: false,
        };
        assert_eq!(
            inline.html_footer().as_deref(),
            Some("Inline images from attachments can\u{2019}t be shown yet.")
        );
    }
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    fn html_mail(body: &str) -> Vec<u8> {
        format!("Content-Type: text/html; charset=utf-8\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn prepare_body_shows_plain_text() {
        let prepared = prepare_body(b"Subject: hi\r\n\r\nHello there", &PrepareOpts::default());
        match prepared {
            PreparedBody::Plain { text, .. } => assert!(text.contains("Hello there")),
            other => panic!("expected Plain, got {other:?}"),
        }
    }

    /// The setting, not the default: after T-141 a reader who turns
    /// "Prefer plain text" back on must still get the plain half.
    #[test]
    fn prepare_body_prefers_plain_in_multipart_alternative() {
        let raw = b"Content-Type: multipart/alternative; boundary=B\r\n\r\n\
                    --B\r\nContent-Type: text/plain\r\n\r\nplain body\r\n\
                    --B\r\nContent-Type: text/html\r\n\r\n<p>html body</p>\r\n\
                    --B--\r\n";
        let opts = PrepareOpts {
            prefer_plain: true,
            ..PrepareOpts::default()
        };
        let prepared = prepare_body(raw, &opts);
        let text = prepared.label_text();
        assert!(text.contains("plain body"), "{text:?}");
        assert!(
            !text.contains("html body") && !text.contains("<p>"),
            "{text:?}"
        );
        assert!(!prepared.is_html());
    }

    #[test]
    fn prepare_body_prefers_html_in_multipart_alternative_when_prefer_plain_is_off() {
        // Same boundary=B fixture as the default-ON test above. Hardcoding
        // `parse_message(bytes, true)` must fail THIS test: the OFF branch
        // is sanitized HTML, not a default change.
        let raw = b"Content-Type: multipart/alternative; boundary=B\r\n\r\n\
                    --B\r\nContent-Type: text/plain\r\n\r\nplain body\r\n\
                    --B\r\nContent-Type: text/html\r\n\r\n<p>html body</p>\r\n\
                    --B--\r\n";
        let prepared = prepare_body(
            raw,
            &PrepareOpts {
                prefer_plain: false,
                ..PrepareOpts::default()
            },
        );
        match &prepared {
            PreparedBody::Html { sanitized, .. } => {
                assert!(
                    sanitized.contains("html body"),
                    "OFF must show the html alternative: {sanitized:?}"
                );
            }
            other => panic!("expected Html, got {other:?}"),
        }
        assert!(prepared.is_html());
        assert!(
            prepared.label_text().is_empty(),
            "HTML occupies the WebView, not the plain-text label: {:?}",
            prepared.label_text()
        );
    }

    #[test]
    fn prepare_body_html_only_is_html_regardless_of_prefer_plain() {
        let raw = html_mail("<p>secret html content</p>");
        for prefer_plain in [true, false] {
            let prepared = prepare_body(
                &raw,
                &PrepareOpts {
                    prefer_plain,
                    ..PrepareOpts::default()
                },
            );
            match &prepared {
                PreparedBody::Html { sanitized, .. } => {
                    assert!(
                        sanitized.contains("secret html content"),
                        "HTML-only mail must show regardless of prefer_plain={prefer_plain}: {sanitized:?}"
                    );
                }
                other => panic!(
                    "HTML-only mail must be Html with prefer_plain={prefer_plain}, got {other:?}"
                ),
            }
            assert!(prepared.is_html());
        }
    }

    #[test]
    fn prepare_body_forwards_prefer_plain_not_a_hardcoded_true() {
        let src = include_str!("html_view.rs");
        let body = extract_brace_body(
            src,
            "pub fn prepare_body(bytes: &[u8], opts: &PrepareOpts) -> PreparedBody {",
        );
        assert!(
            body.contains("opts.prefer_plain"),
            "prepare_body must pass the setting into parse_message"
        );
        assert!(
            !body.contains("parse_message(bytes, true)"),
            "hardcoding prefer_plain = true ignores the Privacy toggle"
        );
    }

    #[test]
    fn prepare_body_sanitizes_html_only_mail_and_keeps_the_text() {
        let raw = html_mail("<p>secret html content</p><script>evil()</script>");
        let prepared = prepare_body(&raw, &PrepareOpts::default());
        match prepared {
            PreparedBody::Html { sanitized, .. } => {
                assert!(
                    sanitized.contains("secret html content"),
                    "the legitimate text of an HTML-only message must show: {sanitized:?}"
                );
                assert!(
                    !sanitized.to_lowercase().contains("<script"),
                    "script tags must not survive sanitize: {sanitized:?}"
                );
                assert!(
                    !sanitized.contains("evil()"),
                    "script body must be cut with the tag: {sanitized:?}"
                );
            }
            other => panic!("HTML-only mail must become PreparedBody::Html, got {other:?}"),
        }
    }

    #[test]
    fn prepare_body_keeps_safe_author_layout_until_the_webview_wrapper() {
        let raw = html_mail(concat!(
            "<style>",
            ".card{width:600px;background:#fff}",
            "@media screen and (max-width:600px){.card{width:100%}}",
            "@import url(https://tracker.example/mail.css);",
            "</style>",
            r#"<table class="card" width="600" cellpadding="16"><tr>"#,
            r#"<td style="padding:12px;background:#f5f6f8">Body</td>"#,
            "</tr></table>",
        ));
        let prepared = prepare_body(&raw, &PrepareOpts::default());
        let PreparedBody::Html { sanitized, .. } = prepared else {
            panic!("expected Html");
        };

        let wrapped = wrap_sanitized_document(&sanitized, false, HtmlPalette::for_dark(false));
        for kept in [
            ".fm-message .card{width:600px;background:#fff;}",
            "@media screen and (max-width:600px)",
            r#"class="card""#,
            r#"width="600""#,
            r#"cellpadding="16""#,
            "padding:12px",
            "background:#f5f6f8",
        ] {
            assert!(wrapped.contains(kept), "{kept} missing from {wrapped}");
        }
        for dropped in ["@import", "tracker.example", "url("] {
            assert!(
                !wrapped.contains(dropped),
                "{dropped} survived in {wrapped}"
            );
        }
    }

    #[test]
    fn prepare_body_acceptance_fixture_never_reaches_the_renderer() {
        // Same fixture T-030's sanitizer already pins; this pins that
        // the shell path actually calls sanitize, not just parse.
        let raw = html_mail(
            "<p>Hello</p>\
             <script>document.cookie</script>\
             <img src=x onerror=alert(1)>\
             <img src=\"https://track.example.com/open.gif\" width=\"1\" height=\"1\">\
             <a href=\"file:///etc/passwd\">x</a>",
        );
        let prepared = prepare_body(&raw, &PrepareOpts::default());
        let PreparedBody::Html {
            sanitized, report, ..
        } = prepared
        else {
            panic!("expected Html");
        };
        let lower = sanitized.to_lowercase();
        assert!(!lower.contains("<script"), "{sanitized}");
        assert!(!sanitized.contains("document.cookie"), "{sanitized}");
        assert!(!lower.contains("onerror"), "{sanitized}");
        assert!(!sanitized.contains("track.example.com"), "{sanitized}");
        assert!(!sanitized.contains("/etc/passwd"), "{sanitized}");
        assert!(!sanitized.contains("file:"), "{sanitized}");
        assert!(
            sanitized.contains("Hello"),
            "legitimate text lost: {sanitized}"
        );
        assert_eq!(report.blocked_tracking_pixels, 1);
    }

    #[test]
    fn prepare_body_blocks_remote_images_by_default() {
        let raw =
            html_mail("<img src=\"https://example.com/photo.jpg\" width=\"600\" height=\"400\">");
        let prepared = prepare_body(&raw, &PrepareOpts::default());
        assert!(prepared.images_banner().is_some());
        let PreparedBody::Html {
            sanitized,
            report,
            allow_remote_images,
            ..
        } = &prepared
        else {
            panic!("expected Html");
        };
        assert!(!sanitized.contains("example.com"), "{sanitized}");
        assert_eq!(report.blocked_remote_images, 1);
        assert!(!*allow_remote_images);
    }

    #[test]
    fn prepare_body_keeps_remote_images_when_allowed() {
        let raw =
            html_mail("<img src=\"https://example.com/photo.jpg\" width=\"600\" height=\"400\">");
        let prepared = prepare_body(
            &raw,
            &PrepareOpts {
                allow_remote_images: true,
                ..PrepareOpts::default()
            },
        );
        assert!(prepared.images_banner().is_none());
        let PreparedBody::Html {
            sanitized, report, ..
        } = &prepared
        else {
            panic!("expected Html");
        };
        assert!(sanitized.contains("example.com/photo.jpg"), "{sanitized}");
        assert_eq!(report.blocked_remote_images, 0);
    }

    #[test]
    fn prepare_body_keeps_a_tracker_only_when_the_toggle_is_off_and_images_are_allowed() {
        let raw =
            html_mail("<img src=\"https://example.com/beacon.gif\" width=\"1\" height=\"1\">");
        let blocked = prepare_body(
            &raw,
            &PrepareOpts {
                allow_remote_images: true,
                ..PrepareOpts::default()
            },
        );
        let PreparedBody::Html {
            sanitized, report, ..
        } = &blocked
        else {
            panic!("expected Html");
        };
        assert!(
            !sanitized.contains("beacon.gif"),
            "D44: tracking pixels stay blocked even with images on: {sanitized}"
        );
        assert_eq!(report.blocked_tracking_pixels, 1);

        let allowed = prepare_body(
            &raw,
            &PrepareOpts {
                allow_remote_images: true,
                block_tracking_pixels: false,
                ..PrepareOpts::default()
            },
        );
        let PreparedBody::Html {
            sanitized, report, ..
        } = &allowed
        else {
            panic!("expected Html");
        };
        assert!(
            sanitized.contains("beacon.gif"),
            "the Privacy toggle is the only way a 1×1 src survives: {sanitized}"
        );
        assert_eq!(report.blocked_tracking_pixels, 0);
    }

    #[test]
    fn prepare_opts_allow_remote_is_the_or_of_the_global_and_per_message_flags() {
        assert!(
            !prepare_opts(true, true, false, true).allow_remote_images,
            "blocked globally, no per-message override"
        );
        assert!(
            prepare_opts(true, true, true, true).allow_remote_images,
            "Show images on this message overrides the global block"
        );
        assert!(
            prepare_opts(false, true, false, true).allow_remote_images,
            "global block off means images load without a per-message click"
        );
        assert!(prepare_opts(true, true, false, true).block_tracking_pixels);
        assert!(!prepare_opts(true, false, false, true).block_tracking_pixels);
    }

    #[test]
    fn a_remembered_sender_domain_counts_as_show_images() {
        let mut allowed = std::collections::HashSet::new();
        allowed.insert("example.com".into());
        assert!(
            allow_images_for_sender(false, Some("example.com"), &allowed),
            "a later letter from the same From host must not wait for a click"
        );
        assert!(
            !allow_images_for_sender(false, Some("other.com"), &allowed),
            "a different sender stays blocked"
        );
        assert!(allow_images_for_sender(true, Some("other.com"), &allowed));
        assert!(!allow_images_for_sender(false, None, &allowed));
        assert!(
            prepare_opts(
                true,
                true,
                allow_images_for_sender(false, Some("example.com"), &allowed),
                true
            )
            .allow_remote_images
        );
    }

    #[test]
    fn prepare_opts_passes_prefer_plain_through() {
        assert!(prepare_opts(true, true, false, true).prefer_plain);
        assert!(!prepare_opts(true, true, false, false).prefer_plain);
        assert!(
            !PrepareOpts::default().prefer_plain,
            "T-141 retires Fork D-plain-default: the sanitized HTML half is \
             what a letter looks like"
        );
    }

    #[test]
    fn prepare_body_debug_never_contains_html_content() {
        let raw = html_mail("<p>canary-body-xyz</p>");
        let prepared = prepare_body(&raw, &PrepareOpts::default());
        let debug = format!("{prepared:?}");
        assert!(
            !debug.contains("canary-body-xyz"),
            "PreparedBody Debug leaked message content: {debug}"
        );
    }

    #[test]
    fn wrap_injects_csp_without_script_src() {
        let wrapped = wrap_sanitized_document("<p>hi</p>", false, HtmlPalette::for_dark(false));
        assert!(wrapped.contains("Content-Security-Policy"));
        assert!(wrapped.contains("script-src 'none'"));
        assert!(wrapped.contains("img-src data:"));
        assert!(!wrapped.contains("img-src data: http:"));
        assert!(wrapped.contains("<p>hi</p>"));
        let allowed = wrap_sanitized_document("<p>hi</p>", true, HtmlPalette::for_dark(false));
        assert!(allowed.contains("img-src data: http: https:"));
        // Both policies — images on or off — must keep script-src none.
        // A mutation that only loosens the "images allowed" branch used
        // to leave this test green.
        assert!(
            allowed.contains("script-src 'none'"),
            "allowing images must not loosen script-src: {allowed}"
        );
        assert!(
            !wrapped.contains("script-src 'unsafe-inline'")
                && !allowed.contains("script-src 'unsafe-inline'"),
            "neither policy may opt into inline script"
        );
        assert!(
            wrapped.contains("<body class=\"fm-message\">")
                && allowed.contains("<body class=\"fm-message\">"),
            "the CSS sanitizer scopes every sender selector to this body"
        );
    }

    #[test]
    fn wrap_uses_design_tokens() {
        let light = wrap_sanitized_document("x", false, HtmlPalette::for_dark(false));
        assert!(light.contains("#0b0c0e"), "{light}");
        assert!(light.contains("#1a58f4"), "{light}");
        let dark = wrap_sanitized_document("x", false, HtmlPalette::for_dark(true));
        assert!(dark.contains("#f3f4f6"), "{dark}");
        assert!(dark.contains("#60a5fa"), "{dark}");
    }

    #[test]
    fn link_decision_refuses_file_javascript_and_data() {
        for uri in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,hi",
            "vbscript:x",
            "blob:https://example.com/1",
            "about:blank",
        ] {
            assert_eq!(link_decision(uri, false), LinkDecision::Refuse, "{uri}");
            assert_eq!(
                link_decision(uri, true),
                LinkDecision::Refuse,
                "{uri} with confirm on must still refuse"
            );
        }
    }

    #[test]
    fn link_decision_http_follows_the_setting_and_mailto_always_confirms() {
        assert_eq!(
            link_decision("https://example.com/a", false),
            LinkDecision::Open
        );
        assert_eq!(
            link_decision("http://example.com/a", true),
            LinkDecision::Confirm
        );
        assert_eq!(
            link_decision("mailto:user@example.com", false),
            LinkDecision::Confirm
        );
    }

    #[test]
    fn refuse_toast_does_not_echo_the_path() {
        let text = refuse_link_toast("file:///etc/passwd");
        assert!(!text.contains("passwd"), "{text}");
        assert!(!text.contains("/etc"), "{text}");
        assert!(text.to_lowercase().contains("file"), "{text}");
    }

    #[test]
    fn response_policy_never_allows_file_even_when_images_are_on() {
        assert!(!response_is_allowed("file:///etc/passwd", true));
        assert!(!response_is_allowed("javascript:alert(1)", true));
        assert!(response_is_allowed("about:blank", false));
        assert!(response_is_allowed("data:image/png;base64,AAAA", false));
        assert!(!response_is_allowed("data:text/html;base64,AAAA", true));
        assert!(response_is_allowed("https://example.com/a.png", true));
        assert!(!response_is_allowed("https://example.com/a.png", false));
    }

    #[test]
    fn spawn_prepare_body_returns_before_prepare_finishes() {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK.get_or_init(|| Mutex::new(()));
        let guard = lock.lock().expect("hold the prepare lock on this thread");

        let (returned_tx, returned_rx) = std::sync::mpsc::channel::<()>();
        let (msg_tx, msg_rx) = std::sync::mpsc::channel::<Msg>();

        std::thread::spawn(move || {
            spawn_prepare_body_with(
                b"hello".to_vec(),
                PrepareOpts::default(),
                9,
                |_, _| {
                    let _held = LOCK.get().expect("lock").lock().expect("prepare waits");
                    PreparedBody::Empty { attachments: 0 }
                },
                move |msg| {
                    let _ = msg_tx.send(msg);
                },
            );
            let _ = returned_tx.send(());
        });

        returned_rx.recv_timeout(Duration::from_secs(5)).expect(
            "spawn_prepare_body must return without waiting for prepare_body \
             — it deadlocked against a lock this test thread is holding, \
             meaning sanitize moved back onto the caller's own thread",
        );
        drop(guard);

        let msg = msg_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("BodyPrepared must still arrive once the lock is free");
        match msg {
            Msg::BodyPrepared { gen, .. } => assert_eq!(gen, 9),
            other => panic!("expected BodyPrepared, got {other:?}"),
        }
    }

    #[test]
    fn prepare_body_undecodable_does_not_leak_bytes() {
        let raw = b"Content-Type: text/plain\r\n\
                    Content-Transfer-Encoding: x-mystery-encoding\r\n\r\n\
                    this exact text must never reach the user";
        let prepared = prepare_body(raw, &PrepareOpts::default());
        let text = prepared.label_text();
        assert!(
            !text.contains("this exact text must never reach the user"),
            "{text:?}"
        );
        assert!(text.contains("could not be decoded"), "{text:?}");
    }

    #[test]
    fn prepare_body_mentions_attachment_count_on_plain() {
        let raw = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n\
                    --B\r\nContent-Type: text/plain\r\n\r\nplain body\r\n\
                    --B\r\nContent-Type: application/octet-stream\r\n\
                    Content-Disposition: attachment; filename=\"a.bin\"\r\n\r\n\
                    binary-ish stuff\r\n\
                    --B--\r\n";
        let text = prepare_body(raw, &PrepareOpts::default()).label_text();
        assert!(text.contains("plain body"), "{text:?}");
        assert!(
            text.contains('1') && text.to_lowercase().contains("attachment"),
            "{text:?}"
        );
    }

    fn extract_brace_body<'a>(src: &'a str, marker: &str) -> &'a str {
        let start = src
            .find(marker)
            .unwrap_or_else(|| panic!("{marker} must exist verbatim"));
        let body_start = start + marker.len();
        let mut depth = 1i32;
        let mut end = None;
        for (i, ch) in src[body_start..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(body_start + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.unwrap_or_else(|| panic!("{marker} must have a matching closing brace"));
        &src[body_start..end]
    }
}
