//! Maps a saved account's `provider` string ([`feathermail_core::Core::account_connection`]'s
//! `provider` field: `"generic"` / `"gmail"` / `"microsoft"`) to the auth
//! style a live IMAP session needs (T-078).
//!
//! This mirrors the dispatch onboarding already does for
//! `MailConnector::probe` -- `GenericImapSmtp` (LOGIN, T-018), `GmailImap`
//! (XOAUTH2, T-019), `MicrosoftImap` (XOAUTH2, T-020) -- but does not call
//! `probe` again: that already ran once when the account was added, and
//! re-probing on every sync tick would be a second, redundant TCP round
//! trip for no benefit. The actual mailbox login for the queue worker goes
//! straight through `ImapSession::connect`, which only needs to know the
//! auth *style* (`ImapAuth::Login` vs `ImapAuth::XOauth2`) and which
//! keyring entry to read it from -- exactly what [`ConnectorKind`] answers.

use feathermail_providers::ImapAuth;
use feathermail_security::SecretKind;

/// Which connector family (T-018/T-019/T-020) a saved account was added
/// under, and therefore how [`crate::provider_factory::ImapProviderFactory`]
/// must authenticate to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectorKind {
    /// `GmailImap`'s auth family (T-019): IMAP XOAUTH2 with the saved OAuth
    /// access token.
    Gmail,
    /// `MicrosoftImap`'s auth family (T-020): XOAUTH2 over the same wire
    /// mechanism as Gmail (`crates/providers/src/xoauth2.rs`).
    Microsoft,
    /// T-165: a Gmail mailbox whose bearer token is issued by the desktop
    /// session's account manager (`feathermail_providers::Goa`) instead of
    /// by a Feather Mail OAuth client. On the wire this is identical to
    /// [`Self::Gmail`] -- same XOAUTH2, same hosts, same keyring slot for
    /// the access token -- so it is a separate variant for exactly one
    /// reason: where the *next* token comes from when this one expires.
    /// Folding it into `Gmail` would send it looking for a Google refresh
    /// token that, by design, was never stored (D19a).
    Goa,
    /// `GenericImapSmtp`'s auth family (T-018): plain LOGIN with the saved
    /// password. Also the fallback for any provider string this crate
    /// does not otherwise recognize, same as `Core::account_connection`'s
    /// own default when saving an account.
    Generic,
}

impl ConnectorKind {
    /// `provider` is `AccountConnection::provider` verbatim -- `"generic"`,
    /// `"gmail"`, or `"microsoft"` are the only values `Core::add_account` /
    /// `add_gmail_account` / `add_microsoft_account` ever write, but an
    /// unrecognized string still needs an answer here (never a panic), so
    /// it falls back to [`Self::Generic`].
    pub fn from_provider_str(provider: &str) -> Self {
        match provider {
            "gmail" => Self::Gmail,
            "microsoft" => Self::Microsoft,
            "goa" => Self::Goa,
            _ => Self::Generic,
        }
    }

    /// Which keyring entry ([`feathermail_security::SecretKey`]'s kind)
    /// this connector family authenticates with.
    pub fn secret_kind(self) -> SecretKind {
        match self {
            Self::Gmail | Self::Microsoft | Self::Goa => SecretKind::OauthAccess,
            Self::Generic => SecretKind::Password,
        }
    }

    /// Builds the [`ImapAuth`] `ImapSession::connect` needs, given the
    /// secret this connector family reads out of the keyring
    /// ([`Self::secret_kind`]).
    pub fn imap_auth(self, secret: String) -> ImapAuth {
        match self {
            Self::Gmail | Self::Microsoft | Self::Goa => ImapAuth::XOauth2(secret),
            Self::Generic => ImapAuth::Login(secret),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_providers_map_to_their_connector_family() {
        assert_eq!(
            ConnectorKind::from_provider_str("gmail"),
            ConnectorKind::Gmail
        );
        assert_eq!(
            ConnectorKind::from_provider_str("microsoft"),
            ConnectorKind::Microsoft
        );
        assert_eq!(
            ConnectorKind::from_provider_str("generic"),
            ConnectorKind::Generic
        );
        assert_eq!(ConnectorKind::from_provider_str("goa"), ConnectorKind::Goa);
    }

    #[test]
    fn unknown_provider_string_falls_back_to_generic_not_a_panic() {
        assert_eq!(
            ConnectorKind::from_provider_str("something-else"),
            ConnectorKind::Generic
        );
        assert_eq!(ConnectorKind::from_provider_str(""), ConnectorKind::Generic);
    }

    #[test]
    fn secret_kind_matches_auth_style() {
        assert_eq!(ConnectorKind::Gmail.secret_kind(), SecretKind::OauthAccess);
        assert_eq!(
            ConnectorKind::Microsoft.secret_kind(),
            SecretKind::OauthAccess
        );
        assert_eq!(ConnectorKind::Generic.secret_kind(), SecretKind::Password);
        assert_eq!(ConnectorKind::Goa.secret_kind(), SecretKind::OauthAccess);
    }

    #[test]
    fn imap_auth_matches_connector_family() {
        assert!(matches!(
            ConnectorKind::Gmail.imap_auth("token".into()),
            ImapAuth::XOauth2(t) if t == "token"
        ));
        assert!(matches!(
            ConnectorKind::Microsoft.imap_auth("token".into()),
            ImapAuth::XOauth2(t) if t == "token"
        ));
        assert!(matches!(
            ConnectorKind::Generic.imap_auth("hunter2".into()),
            ImapAuth::Login(p) if p == "hunter2"
        ));
        assert!(matches!(
            ConnectorKind::Goa.imap_auth("token".into()),
            ImapAuth::XOauth2(t) if t == "token"
        ));
    }
}
