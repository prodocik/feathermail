//! GNOME Online Accounts as a token source (T-165, D19a).
//!
//! Why this exists at all: `https://mail.google.com/` is a *restricted*
//! scope, so a Google sign-in button that works out of the box for every
//! user needs a verified client ID plus an annual third-party security
//! assessment. `crates/providers/src/oauth.rs` already implements the
//! loopback+PKCE flow for the day Feather Mail owns such a client ID, and
//! `config/oauth.toml.example` documents the env/file/build-time hooks a
//! distributor or a fork uses to supply their own. This module is the
//! other answer, and the one that needs no client ID from us at all: the
//! desktop session already has an account manager that holds a verified
//! Google grant, and it hands the access token to any application in the
//! session over D-Bus. Evolution has consumed GOA this way for years.
//!
//! D14/D19a: Feather Mail therefore ships **no** Google client secret and
//! stores **no** Google refresh token for these accounts. The only thing
//! saved for a GOA account is the account's GOA *id* (see
//! [`Goa::refresh`]'s doc comment for why that lands in the keyring's
//! `oauth_refresh` slot) plus the short-lived access token cached in
//! `oauth_access`, exactly like the Gmail/Microsoft path already does.
//! Revoking access is done where the account lives -- Settings -> Online
//! Accounts -- not inside Feather Mail.
//!
//! Scope of this module: only the *token*. The mailbox hosts still come
//! from `MailboxForm::gmail`, i.e. the same constants T-019 already probes
//! against, so a GOA account is an ordinary XOAUTH2 IMAP/SMTP account whose
//! bearer token happens to be issued by the session's account manager.

use std::collections::HashMap;

use crate::oauth::{OauthError, TokenRefresh, TokenSet};

pub const GOA_SERVICE: &str = "org.gnome.OnlineAccounts";
pub const GOA_OBJECT_ROOT: &str = "/org/gnome/OnlineAccounts";
pub const GOA_ACCOUNT_IFACE: &str = "org.gnome.OnlineAccounts.Account";
pub const GOA_MAIL_IFACE: &str = "org.gnome.OnlineAccounts.Mail";
pub const GOA_OAUTH2_IFACE: &str = "org.gnome.OnlineAccounts.OAuth2Based";
/// `org.gnome.OnlineAccounts.Account.ProviderType` for a Google account.
/// The only provider type this module accepts today -- see
/// [`usable_account`] for why the others are refused rather than guessed at.
pub const GOA_GOOGLE_PROVIDER_TYPE: &str = "google";

/// One object under [`GOA_OBJECT_ROOT`], flattened out of
/// `GetManagedObjects` into just the properties this crate reads. Kept
/// separate from [`GoaAccount`] so that the D-Bus decoding
/// ([`LiveGoaBus`]) and the "is this account usable for mail" policy
/// ([`usable_account`]) are two testable halves: every rule below is
/// exercised by this module's own tests through [`FakeGoaBus`], with no
/// session bus and no GNOME installed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoaObject {
    pub path: String,
    pub id: String,
    pub provider_type: String,
    pub provider_name: String,
    /// What Settings shows for the account, e.g. `you@gmail.com`. Used
    /// only as a fallback label; the address Feather Mail actually adds
    /// comes from the `Mail` interface's `EmailAddress`.
    pub presentation_identity: String,
    pub mail_disabled: bool,
    pub is_locked: bool,
    pub attention_needed: bool,
    pub has_oauth2: bool,
    pub mail: Option<GoaMail>,
}

/// The `org.gnome.OnlineAccounts.Mail` properties this crate reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoaMail {
    pub email: String,
    pub imap_supported: bool,
    pub imap_host: String,
    pub smtp_supported: bool,
    pub smtp_host: String,
}

/// A GOA account Feather Mail can actually add: a Google account with mail
/// enabled, an OAuth2 token source, and an address to file it under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoaAccount {
    /// `org.gnome.OnlineAccounts.Account.Id` -- stable across restarts,
    /// which is what makes it usable as the saved handle back to this
    /// account (see [`Goa::refresh`]).
    pub id: String,
    pub path: String,
    pub email: String,
    /// Human provider label straight from GOA (`ProviderName`), so the UI
    /// says whatever Settings says rather than a second hardcoded
    /// "Google" that could drift from it.
    pub provider_name: String,
}

/// Why one GOA account is not offered in the picker. Carried as text
/// because every arm ends up in the same place -- a line under the
/// account in the wizard -- and because the distinction is for the human,
/// not for control flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoaSkip {
    pub label: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoaError {
    /// No account manager on the session bus: not installed, or this is
    /// not a session where it runs. Not an error the user did anything
    /// wrong -- the caller hides the button instead of showing this.
    Unavailable { message: String },
    /// The saved account is gone from Settings (or was re-created, which
    /// gives it a new id). Terminal: only a human can fix it.
    NotFound { message: String },
    /// Present but not usable right now: locked, or GOA is asking the
    /// user to sign in again.
    Unusable { message: String },
    /// Anything else the bus reported. `details` may carry D-Bus error
    /// text, so D14 keeps it off the UI boundary -- callers surface
    /// `message` only.
    Bus {
        message: String,
        details: Option<String>,
    },
}

impl GoaError {
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable { message }
            | Self::NotFound { message }
            | Self::Unusable { message }
            | Self::Bus { message, .. } => message,
        }
    }

    /// Maps onto the error type the reauth path already understands
    /// (`OauthError::apply_error` draws the retryable/terminal line).
    ///
    /// The split is deliberate and is *not* the same as "did this fail":
    /// a missing or crashed daemon is a transient condition that the
    /// existing backoff should ride out, so it becomes `Network`
    /// (retryable). An account that no longer exists, or that GOA itself
    /// says needs attention, cannot be fixed by retrying -- only by the
    /// user going to Settings -- so it becomes `Revoked`, which
    /// `apply_error` already turns into a terminal `ApplyError::Auth`.
    pub fn oauth_error(self) -> OauthError {
        match self {
            Self::Unavailable { message } => OauthError::Network {
                message,
                details: None,
            },
            Self::Bus { message, details } => OauthError::Network { message, details },
            Self::NotFound { message } => OauthError::Revoked {
                message,
                details: None,
            },
            Self::Unusable { message } => OauthError::Revoked {
                message,
                details: None,
            },
        }
    }

    /// Deliberately drops the underlying bus text: this is the one error
    /// the UI never shows (the button is hidden instead), so keeping the
    /// detail would only be a D14 liability with no reader.
    fn unavailable() -> Self {
        Self::Unavailable {
            message: "GNOME Online Accounts isn't running in this session.".into(),
        }
    }
}

impl std::fmt::Display for GoaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// The session-bus half, behind a trait so the policy above it is tested
/// without a bus. Mirrors the seam `autoconfig`'s [`crate::HttpGet`] and
/// `oauth`'s [`crate::HttpForm`] already set in this crate: one narrow
/// trait for the I/O, everything else pure.
pub trait GoaBus: Send {
    fn objects(&self) -> Result<Vec<GoaObject>, GoaError>;
    /// `org.gnome.OnlineAccounts.OAuth2Based.GetAccessToken` on `path`.
    /// May block for a network round trip -- GOA refreshes the token
    /// itself when the cached one has expired -- so this never runs on
    /// the GTK thread (D11); its callers are the provisioning thread and
    /// the sync worker.
    fn access_token(&self, path: &str) -> Result<String, GoaError>;
}

/// Decides whether one GOA object is an account Feather Mail can add, and
/// says why not when it isn't.
///
/// Refusing an unknown `ProviderType` rather than trying it is the whole
/// point: GOA also carries Nextcloud, Fastmail-style IMAP accounts and
/// Microsoft accounts, whose token semantics and hosts this crate has not
/// probed. T-019's Gmail path is the one with a tested XOAUTH2 probe and
/// tested hosts behind it, so `google` is what is offered; anything else
/// keeps the honest "not supported here" line and the manual form.
pub fn usable_account(object: &GoaObject) -> Result<GoaAccount, GoaSkip> {
    let label = if object.presentation_identity.is_empty() {
        object.id.clone()
    } else {
        object.presentation_identity.clone()
    };
    let skip = |reason: &str| {
        Err(GoaSkip {
            label: label.clone(),
            reason: reason.to_string(),
        })
    };

    if object.provider_type != GOA_GOOGLE_PROVIDER_TYPE {
        return skip("Only Google accounts are supported here yet. Use Other IMAP.");
    }
    if !object.has_oauth2 {
        return skip("This account has no OAuth2 token to share.");
    }
    if object.is_locked {
        return skip("Unlock this account in Settings first.");
    }
    if object.attention_needed {
        return skip("Sign in to this account again in Settings.");
    }
    if object.mail_disabled {
        return skip("Mail is turned off for this account in Settings.");
    }
    let Some(mail) = object.mail.as_ref() else {
        return skip("This account doesn't offer mail.");
    };
    if !mail.imap_supported || !mail.smtp_supported {
        return skip("This account doesn't offer IMAP and SMTP.");
    }
    let email = if mail.email.is_empty() {
        object.presentation_identity.clone()
    } else {
        mail.email.clone()
    };
    if email.is_empty() {
        return skip("This account has no email address.");
    }
    Ok(GoaAccount {
        id: object.id.clone(),
        path: object.path.clone(),
        email,
        provider_name: if object.provider_name.is_empty() {
            "Google".to_string()
        } else {
            object.provider_name.clone()
        },
    })
}

/// The account manager, over whichever [`GoaBus`] it was built with.
pub struct Goa<B> {
    bus: B,
}

impl<B: GoaBus> Goa<B> {
    pub fn new(bus: B) -> Self {
        Self { bus }
    }

    /// Every account that can be added right now, plus the ones that were
    /// skipped and why. Both halves are returned because the wizard shows
    /// them together: hiding a Google account that merely needs unlocking
    /// would read as "Feather Mail can't see my account".
    pub fn mail_accounts(&self) -> Result<(Vec<GoaAccount>, Vec<GoaSkip>), GoaError> {
        let mut usable = Vec::new();
        let mut skipped = Vec::new();
        for object in self.bus.objects()? {
            match usable_account(&object) {
                Ok(account) => usable.push(account),
                Err(skip) => skipped.push(skip),
            }
        }
        usable.sort_by(|a, b| a.email.cmp(&b.email));
        Ok((usable, skipped))
    }

    /// Resolve a saved handle back to a live account. `handle` is a GOA
    /// account id; the email fallback covers the one case an id cannot:
    /// the user removed and re-added the same address in Settings, which
    /// gives GOA a fresh id for what is, to the person, the same account.
    pub fn find(&self, handle: &str) -> Result<GoaAccount, GoaError> {
        let (usable, _) = self.mail_accounts()?;
        if let Some(found) = usable.iter().find(|account| account.id == handle) {
            return Ok(found.clone());
        }
        if let Some(found) = usable
            .iter()
            .find(|account| account.email.eq_ignore_ascii_case(handle))
        {
            return Ok(found.clone());
        }
        Err(GoaError::NotFound {
            message: "This account is no longer in Settings -> Online Accounts.".into(),
        })
    }

    pub fn access_token(&self, account: &GoaAccount) -> Result<String, GoaError> {
        self.bus.access_token(&account.path)
    }
}

impl Goa<LiveGoaBus> {
    /// Dial the session bus. `Err(GoaError::Unavailable)` is the ordinary
    /// answer on a session without GOA, not an exceptional one.
    pub fn connect() -> Result<Self, GoaError> {
        Ok(Self::new(LiveGoaBus::connect()?))
    }

    /// Whether this session has any addable GOA mail account. Used by the
    /// wizard to decide if the button exists at all, so every failure --
    /// no daemon, no accounts, a bus error -- collapses to `false` rather
    /// than to an error the user cannot act on.
    pub fn available() -> bool {
        Self::connect()
            .and_then(|goa| goa.mail_accounts())
            .map(|(usable, _)| !usable.is_empty())
            .unwrap_or(false)
    }
}

/// Lets a GOA account reuse T-083's reauth machinery unchanged.
///
/// The handle in the `oauth_refresh` keyring slot is a GOA account id, not
/// a Google refresh token, and that is deliberate: `OauthReauth` already
/// implements exactly the sequence a GOA account needs -- read the saved
/// handle, exchange it for a fresh access token, persist that token, dial
/// a new session -- and the only provider-specific part is this one
/// method. Writing a second, near-identical `Reauthenticate` for GOA
/// would duplicate that flow (the mistake T-020 already made once with
/// `microsoft.rs` and had to undo). What lands in the keyring is
/// therefore not a secret at all but a capability reference; nothing here
/// weakens D14, because the token it yields is exactly as short-lived as
/// the Gmail path's and the long-lived grant stays in GOA's own storage.
///
/// `refresh_token: None` in the returned set is load-bearing:
/// `OauthReauth` only overwrites the saved handle when a rotated value
/// comes back, so returning `None` leaves the GOA id in place.
impl<B: GoaBus> TokenRefresh for Goa<B> {
    fn refresh(&self, handle: &str) -> Result<TokenSet, OauthError> {
        let account = self.find(handle).map_err(GoaError::oauth_error)?;
        let access_token = self.access_token(&account).map_err(GoaError::oauth_error)?;
        Ok(TokenSet {
            access_token,
            refresh_token: None,
            expires_in: None,
        })
    }
}

/// The real session bus, via `GetManagedObjects` on GOA's object root --
/// one round trip for every account and every interface, instead of a
/// property read per account per interface.
pub struct LiveGoaBus {
    connection: zbus::blocking::Connection,
}

impl LiveGoaBus {
    pub fn connect() -> Result<Self, GoaError> {
        let connection =
            zbus::blocking::Connection::session().map_err(|_| GoaError::unavailable())?;
        Ok(Self { connection })
    }

    fn proxy<'a>(
        &'a self,
        path: &'a str,
        interface: &'a str,
    ) -> Result<zbus::blocking::Proxy<'a>, GoaError> {
        zbus::blocking::Proxy::new(&self.connection, GOA_SERVICE, path, interface).map_err(|err| {
            GoaError::Bus {
                message: "Couldn't talk to GNOME Online Accounts.".into(),
                details: Some(err.to_string()),
            }
        })
    }
}

type ManagedObjects = HashMap<
    zbus::zvariant::OwnedObjectPath,
    HashMap<String, HashMap<String, zbus::zvariant::OwnedValue>>,
>;

impl GoaBus for LiveGoaBus {
    fn objects(&self) -> Result<Vec<GoaObject>, GoaError> {
        let proxy = self.proxy(GOA_OBJECT_ROOT, "org.freedesktop.DBus.ObjectManager")?;
        let managed: ManagedObjects = proxy
            .call("GetManagedObjects", &())
            .map_err(|_| GoaError::unavailable())?;
        let mut objects: Vec<GoaObject> = managed
            .into_iter()
            .filter_map(|(path, interfaces)| decode_object(path.as_str(), &interfaces))
            .collect();
        // `GetManagedObjects` has no defined order; a stable one keeps the
        // wizard's list from reshuffling between openings.
        objects.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(objects)
    }

    fn access_token(&self, path: &str) -> Result<String, GoaError> {
        let proxy = self.proxy(path, GOA_OAUTH2_IFACE)?;
        let (access_token, _expires_in): (String, i32) =
            proxy.call("GetAccessToken", &()).map_err(|err| {
                let text = err.to_string();
                // GOA answers a removed account with UnknownObject/
                // UnknownMethod; anything else (including "the user has to
                // sign in again") is reported as a plain failure, so the
                // conservative reading is "try again later" for the rest.
                if text.contains("UnknownObject") || text.contains("UnknownMethod") {
                    GoaError::NotFound {
                        message: "This account is no longer in Settings -> Online Accounts.".into(),
                    }
                } else {
                    GoaError::Bus {
                        message: "GNOME Online Accounts didn't hand over a token.".into(),
                        details: Some(text),
                    }
                }
            })?;
        if access_token.is_empty() {
            return Err(GoaError::Unusable {
                message: "GNOME Online Accounts returned an empty token.".into(),
            });
        }
        Ok(access_token)
    }
}

/// Flattens one `GetManagedObjects` entry. Returns `None` for objects that
/// are not accounts at all (GOA also exports `/Manager`).
fn decode_object(
    path: &str,
    interfaces: &HashMap<String, HashMap<String, zbus::zvariant::OwnedValue>>,
) -> Option<GoaObject> {
    let account = interfaces.get(GOA_ACCOUNT_IFACE)?;
    let mail = interfaces.get(GOA_MAIL_IFACE).map(|props| GoaMail {
        email: string_prop(props, "EmailAddress"),
        imap_supported: bool_prop(props, "ImapSupported"),
        imap_host: string_prop(props, "ImapHost"),
        smtp_supported: bool_prop(props, "SmtpSupported"),
        smtp_host: string_prop(props, "SmtpHost"),
    });
    Some(GoaObject {
        path: path.to_string(),
        id: string_prop(account, "Id"),
        provider_type: string_prop(account, "ProviderType"),
        provider_name: string_prop(account, "ProviderName"),
        presentation_identity: string_prop(account, "PresentationIdentity"),
        mail_disabled: bool_prop(account, "MailDisabled"),
        is_locked: bool_prop(account, "IsLocked"),
        attention_needed: bool_prop(account, "AttentionNeeded"),
        has_oauth2: interfaces.contains_key(GOA_OAUTH2_IFACE),
        mail,
    })
}

/// A property GOA did not send, or sent with an unexpected type, reads as
/// empty rather than aborting the whole listing: one odd account must not
/// hide the others. The policy above treats empty as "not usable", so a
/// missing `Id` or address never produces a half-built account.
fn string_prop(props: &HashMap<String, zbus::zvariant::OwnedValue>, key: &str) -> String {
    props
        .get(key)
        .and_then(|value| String::try_from(value.clone()).ok())
        .unwrap_or_default()
}

/// Same tolerance as [`string_prop`], and the safe default differs per
/// property: every boolean read here is a *negative* (`MailDisabled`,
/// `IsLocked`, `AttentionNeeded`) except the two `*Supported` flags, so
/// `false` is the conservative answer in both directions -- a missing
/// `Supported` flag means "don't offer it", a missing `Disabled` flag
/// means "not disabled", which is what GOA itself defaults them to.
fn bool_prop(props: &HashMap<String, zbus::zvariant::OwnedValue>, key: &str) -> bool {
    props
        .get(key)
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bus with no daemon behind it: the objects are whatever the test
    /// says they are, and `access_token` answers per path.
    struct FakeGoaBus {
        objects: Vec<GoaObject>,
        tokens: HashMap<String, Result<String, GoaError>>,
    }

    impl FakeGoaBus {
        fn with(objects: Vec<GoaObject>) -> Self {
            Self {
                objects,
                tokens: HashMap::new(),
            }
        }

        fn token(mut self, path: &str, token: &str) -> Self {
            self.tokens.insert(path.to_string(), Ok(token.to_string()));
            self
        }
    }

    impl GoaBus for FakeGoaBus {
        fn objects(&self) -> Result<Vec<GoaObject>, GoaError> {
            Ok(self.objects.clone())
        }

        fn access_token(&self, path: &str) -> Result<String, GoaError> {
            self.tokens
                .get(path)
                .cloned()
                .unwrap_or(Err(GoaError::NotFound {
                    message: "gone".into(),
                }))
        }
    }

    fn google(id: &str, email: &str) -> GoaObject {
        GoaObject {
            path: format!("/org/gnome/OnlineAccounts/Accounts/{id}"),
            id: id.to_string(),
            provider_type: "google".into(),
            provider_name: "Google".into(),
            presentation_identity: email.to_string(),
            mail_disabled: false,
            is_locked: false,
            attention_needed: false,
            has_oauth2: true,
            mail: Some(GoaMail {
                email: email.to_string(),
                imap_supported: true,
                imap_host: "imap.gmail.com".into(),
                smtp_supported: true,
                smtp_host: "smtp.gmail.com".into(),
            }),
        }
    }

    #[test]
    fn a_healthy_google_account_is_offered() {
        let account = usable_account(&google("account_1", "you@gmail.com")).unwrap();
        assert_eq!(account.id, "account_1");
        assert_eq!(account.email, "you@gmail.com");
        assert_eq!(account.provider_name, "Google");
    }

    /// Each refusal is pinned separately: a single "is it usable" test
    /// would stay green if one of these rules were deleted.
    #[test]
    fn a_non_google_account_is_refused() {
        let mut object = google("account_1", "you@example.com");
        object.provider_type = "imap_smtp".into();
        let skip = usable_account(&object).unwrap_err();
        assert!(skip.reason.contains("Only Google"), "{skip:?}");
    }

    #[test]
    fn an_account_without_oauth2_is_refused() {
        let mut object = google("account_1", "you@gmail.com");
        object.has_oauth2 = false;
        assert!(usable_account(&object).is_err());
    }

    #[test]
    fn a_locked_account_is_refused() {
        let mut object = google("account_1", "you@gmail.com");
        object.is_locked = true;
        let skip = usable_account(&object).unwrap_err();
        assert!(skip.reason.contains("Unlock"), "{skip:?}");
    }

    #[test]
    fn an_account_needing_attention_is_refused() {
        let mut object = google("account_1", "you@gmail.com");
        object.attention_needed = true;
        let skip = usable_account(&object).unwrap_err();
        assert!(skip.reason.contains("Sign in"), "{skip:?}");
    }

    #[test]
    fn an_account_with_mail_turned_off_is_refused() {
        let mut object = google("account_1", "you@gmail.com");
        object.mail_disabled = true;
        let skip = usable_account(&object).unwrap_err();
        assert!(skip.reason.contains("Mail is turned off"), "{skip:?}");
    }

    #[test]
    fn an_account_without_imap_or_smtp_is_refused() {
        let mut object = google("account_1", "you@gmail.com");
        object.mail.as_mut().unwrap().smtp_supported = false;
        assert!(usable_account(&object).is_err());
    }

    #[test]
    fn the_presentation_identity_stands_in_for_a_missing_mail_address() {
        let mut object = google("account_1", "you@gmail.com");
        object.mail.as_mut().unwrap().email = String::new();
        assert_eq!(usable_account(&object).unwrap().email, "you@gmail.com");
    }

    #[test]
    fn skipped_accounts_are_listed_next_to_usable_ones() {
        let mut locked = google("account_2", "other@gmail.com");
        locked.is_locked = true;
        let goa = Goa::new(FakeGoaBus::with(vec![
            google("account_1", "you@gmail.com"),
            locked,
        ]));
        let (usable, skipped) = goa.mail_accounts().unwrap();
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].email, "you@gmail.com");
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].label, "other@gmail.com");
    }

    #[test]
    fn accounts_come_back_in_a_stable_order() {
        let goa = Goa::new(FakeGoaBus::with(vec![
            google("account_2", "zoe@gmail.com"),
            google("account_1", "amy@gmail.com"),
        ]));
        let (usable, _) = goa.mail_accounts().unwrap();
        let emails: Vec<&str> = usable.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(emails, ["amy@gmail.com", "zoe@gmail.com"]);
    }

    #[test]
    fn a_saved_id_resolves_back_to_its_account() {
        let goa = Goa::new(FakeGoaBus::with(vec![google("account_1", "you@gmail.com")]));
        assert_eq!(goa.find("account_1").unwrap().email, "you@gmail.com");
    }

    /// Re-adding the same address in Settings gives GOA a new id; the
    /// address is what the person considers stable, so it is the fallback.
    #[test]
    fn a_saved_id_that_changed_falls_back_to_the_address() {
        let goa = Goa::new(FakeGoaBus::with(vec![google("account_9", "you@gmail.com")]));
        let found = goa.find("you@gmail.com").unwrap();
        assert_eq!(found.id, "account_9");
    }

    #[test]
    fn a_removed_account_is_not_found() {
        let goa = Goa::new(FakeGoaBus::with(vec![]));
        let err = goa.find("account_1").unwrap_err();
        assert!(matches!(err, GoaError::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn refresh_hands_back_a_token_without_touching_the_saved_handle() {
        let goa = Goa::new(
            FakeGoaBus::with(vec![google("account_1", "you@gmail.com")])
                .token("/org/gnome/OnlineAccounts/Accounts/account_1", "ya29.fresh"),
        );
        let tokens = goa.refresh("account_1").unwrap();
        assert_eq!(tokens.access_token, "ya29.fresh");
        // `OauthReauth` overwrites the saved handle only when a rotated
        // value comes back -- `None` is what keeps the GOA id in place.
        assert_eq!(tokens.refresh_token, None);
    }

    /// A daemon that is merely down must stay retryable, or one crashed
    /// session would mark every GOA account as needing a re-login.
    #[test]
    fn a_missing_daemon_is_a_retryable_network_error() {
        let err = GoaError::unavailable().oauth_error();
        assert!(matches!(err, OauthError::Network { .. }), "{err:?}");
    }

    #[test]
    fn a_removed_account_is_a_terminal_error() {
        let err = GoaError::NotFound {
            message: "gone".into(),
        }
        .oauth_error();
        assert!(matches!(err, OauthError::Revoked { .. }), "{err:?}");
    }

    #[test]
    fn decoding_skips_objects_that_are_not_accounts() {
        let interfaces = HashMap::from([(
            "org.gnome.OnlineAccounts.Manager".to_string(),
            HashMap::new(),
        )]);
        assert!(decode_object("/org/gnome/OnlineAccounts/Manager", &interfaces).is_none());
    }

    #[test]
    fn decoding_survives_a_property_goa_did_not_send() {
        let account = HashMap::from([(
            "Id".to_string(),
            zbus::zvariant::OwnedValue::from(zbus::zvariant::Str::from("account_1")),
        )]);
        let interfaces = HashMap::from([(GOA_ACCOUNT_IFACE.to_string(), account)]);
        let object = decode_object("/p", &interfaces).unwrap();
        assert_eq!(object.id, "account_1");
        assert!(object.provider_type.is_empty());
        assert!(!object.mail_disabled);
        assert!(object.mail.is_none());
        // And an object decoded from that half-empty state is refused
        // rather than added as a broken account.
        assert!(usable_account(&object).is_err());
    }
}
