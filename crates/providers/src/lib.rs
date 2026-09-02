//! IMAP/SMTP provider implementations (T-018, T-019).
//!
//! UI, MCP, and shortcuts talk to Core. This crate does not import GTK.

mod apply;
mod autoconfig;
mod folders;
mod generic;
mod gmail;
mod goa;
mod idle;
mod microsoft;
mod oauth;
mod send;
mod session;
mod sync_session;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod wire;
mod xoauth2;

pub use apply::{
    ImapMailProvider, RemoteLocator, RemoteMessage, ARCHIVE_FOLDER_KEY, TRASH_FOLDER_KEY,
};
pub use autoconfig::{
    lookup as autoconfig_lookup, parse_ispdb, AutoconfigError, DnsSrv, HttpGet, LiveDns, LiveHttp,
    SrvRecord,
};
pub use folders::discovered_folders;
pub use generic::GenericImapSmtp;
pub use gmail::GmailImap;
pub use goa::{
    usable_account, Goa, GoaAccount, GoaBus, GoaError, GoaMail, GoaObject, GoaSkip, LiveGoaBus,
    GOA_ACCOUNT_IFACE, GOA_GOOGLE_PROVIDER_TYPE, GOA_MAIL_IFACE, GOA_OAUTH2_IFACE, GOA_OBJECT_ROOT,
    GOA_SERVICE,
};
pub use idle::{run_idle, run_idle_with, IdleOutcome, IDLE_TIMEOUT_SECS, NO_IDLE_POLL_SECS};
pub use microsoft::{
    MicrosoftClientConfig, MicrosoftImap, MicrosoftOauth, MICROSOFT_AUTH_URL, MICROSOFT_SCOPE,
    MICROSOFT_TOKEN_URL,
};
pub use oauth::{
    google_account_email, open_browser, sasl_xoauth2, AuthSession, GoogleClientConfig, GoogleOauth,
    HttpForm, OauthError, OauthReauth, Pkce, TokenRefresh, TokenSet, GMAIL_SCOPE, GOOGLE_AUTH_URL,
    GOOGLE_SCOPES, GOOGLE_TOKEN_URL, GOOGLE_USERINFO_URL,
};
pub use send::{
    build_draft_message, build_outbox_message, send_formatted, send_message, send_outbox, SmtpAuth,
};
pub use session::{
    AttachmentFetchLimits, Capabilities, FolderListing, HeaderMeta, IdleEvent, ImapAuth,
    ImapSession, SelectedMailbox, UidRange,
};

/// Workspace probe so `cargo test -p feathermail-providers` has a test.
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::crate_name;

    #[test]
    fn crate_compiles() {
        assert!(crate_name().starts_with("feathermail-"));
    }
}
