//! Best-effort tracking-pixel detection (T-030).
//!
//! This module answers one narrow question: "does this `<img>` look like a
//! tracking beacon rather than real content?" It is deliberately a
//! heuristic, not a security boundary — the security boundary is the
//! allow-list in `sanitize.rs` (script/on*/javascript:/file:/data: are gone
//! regardless of anything in this file). This module only decides whether
//! to also drop an *otherwise-permitted* `http(s)` image even when the user
//! has opted in to remote images (D44: "Tracking pixels: резать 1×1 и
//! известные tracker host, дефолт ON" — "always", not "unless allowed").
//!
//! Two independent signals, either one is enough:
//!
//! 1. **Declared size.** `width="1"` or `height="1"` (or `"0"`, or the same
//!    values with a `px` suffix) on an `<img>`, either as an HTML attribute
//!    or in its parsed, allow-listed inline CSS. Real content images are
//!    essentially never declared at 1x1 or 0x0; tracking beacons are
//!    *always* declared at 1x1 by convention (so mail clients that do
//!    render them don't visibly break the layout). We check either
//!    dimension alone, not both together, and treat "0" the same as "1":
//!    a sender who wanted a real inline separator would not pick a
//!    dimension of zero or one pixel.
//! 2. **Known tracker host.** A small, static, hand-maintained list of
//!    email-service-provider tracking domains (open/click tracking
//!    redirectors and pixel hosts). Matched by exact host or host suffix
//!    (`foo.list-manage.com` matches `list-manage.com`).
//!
//! What this heuristic does **not** catch (documented honestly, see
//! `docs/plan.md` T-030): a pixel declared with real-looking dimensions
//! (e.g. `width="20" height="20"`) served from a host not on the static
//! list; a pixel whose CSS computes to 1px (`width:calc(1px)`); or a tracker
//! fronted by a fresh/unlisted domain. None of those bypass the
//! *safety* guarantees (no script execution, no `file://`, no remote load
//! when `allow_remote_images` is off) — they only mean a permitted, opted-in
//! remote image load might carry a tracking pixel we didn't flag.

/// Domains (or domain suffixes) known to serve open/click tracking pixels
/// for common email service providers. Not exhaustive — see module docs.
/// Static data only (D9): no lookups, no network calls.
pub(crate) const KNOWN_TRACKER_HOSTS: &[&str] = &[
    // Mailchimp / Mandrill
    "list-manage.com",
    "list-manage1.com",
    "mailchimp.com",
    "mandrillapp.com",
    // SendGrid
    "sendgrid.net",
    "sendgrid.com",
    // HubSpot
    "hs-analytics.net",
    "hubspotemail.net",
    "hubspotlinks.com",
    // Web analytics beacons sometimes embedded as 1x1 <img> in HTML mail
    "google-analytics.com",
    "doubleclick.net",
    "googletagmanager.com",
    "facebook.com",
    "px.ads.linkedin.com",
    "bat.bing.com",
    // Klaviyo
    "klaviyomail.com",
    "trk.klclick.com",
    // Sailthru / Salesforce Marketing Cloud / Pardot
    "sailthru.com",
    "pardot.com",
    "exacttarget.com",
    // Misc ESPs
    "mailtrack.io",
    "getvero.com",
    "constantcontact.com",
    "campaign-archive.com",
    "sparkpostmail.com",
    "mailgun.org",
    "mailjet.com",
];

/// True if `host` is, or is a subdomain of, a known tracker host.
pub(crate) fn is_known_tracker_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    KNOWN_TRACKER_HOSTS
        .iter()
        .any(|known| host == *known || host.ends_with(&format!(".{known}")))
}

/// True if an image `width`/`height` attribute value reads as "1 pixel or
/// less" — `"1"`, `"0"`, `"1px"`, `"0px"`, with optional surrounding
/// whitespace. Anything else (including unparsable garbage, percentages,
/// or `"auto"`) is *not* flagged: false negatives here only mean we fall
/// through to the host check, never a crash.
pub(crate) fn declares_tiny_dimension(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    let value = value.strip_suffix("!important").unwrap_or(&value).trim();
    let value = value.strip_suffix("px").unwrap_or(value).trim();
    matches!(value, "0" | "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_dimension_matches_one_and_zero_with_or_without_px() {
        for v in ["1", "0", "1px", "0px", " 1 ", "1PX"] {
            assert!(declares_tiny_dimension(v), "expected tiny: {v:?}");
        }
    }

    #[test]
    fn tiny_dimension_rejects_real_sizes_and_garbage() {
        for v in ["2", "20", "1%", "100%", "1foo", "auto", "", "px", "-1"] {
            assert!(!declares_tiny_dimension(v), "expected not tiny: {v:?}");
        }
    }

    #[test]
    fn tracker_host_matches_exact_and_subdomain() {
        assert!(is_known_tracker_host("list-manage.com"));
        assert!(is_known_tracker_host("us1.list-manage.com"));
        assert!(is_known_tracker_host("US1.LIST-MANAGE.COM"));
        assert!(is_known_tracker_host("list-manage.com."));
    }

    #[test]
    fn tracker_host_rejects_unrelated_and_lookalike_hosts() {
        assert!(!is_known_tracker_host("example.com"));
        // Must not match as a substring — "evil-list-manage.com" is a
        // different registrable domain, not a subdomain of list-manage.com.
        assert!(!is_known_tracker_host("evil-list-manage.com"));
        assert!(!is_known_tracker_host("list-manage.com.evil.com"));
    }
}
