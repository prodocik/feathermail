//! Add-account form (T-017). Probe is [`crate::MailConnector`] (T-018).
//!
//! Password is validated, never stored on this type (D14).

use std::collections::HashSet;

use crate::model::AccountId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailSecurity {
    StartTls,
    Ssl,
    None,
}

impl MailSecurity {
    pub const ALL: [Self; 3] = [Self::StartTls, Self::Ssl, Self::None];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::StartTls => "STARTTLS",
            Self::Ssl => "SSL",
            Self::None => "None",
        }
    }

    pub fn from_index(i: u32) -> Self {
        match i {
            1 => Self::Ssl,
            2 => Self::None,
            _ => Self::StartTls,
        }
    }

    pub fn index(self) -> u32 {
        match self {
            Self::StartTls => 0,
            Self::Ssl => 1,
            Self::None => 2,
        }
    }

    pub fn default_imap_port(self) -> u16 {
        match self {
            Self::Ssl => 993,
            Self::StartTls | Self::None => 143,
        }
    }

    pub fn default_smtp_port(self) -> u16 {
        match self {
            Self::Ssl => 465,
            Self::StartTls => 587,
            Self::None => 25,
        }
    }

    /// Round-trip of [`Self::as_str`], for reading the `accounts` row back (T-021).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "STARTTLS" => Some(Self::StartTls),
            "SSL" => Some(Self::Ssl),
            "None" => Some(Self::None),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailboxFormError {
    Email,
    Password,
    ImapHost,
    SmtpHost,
    ImapPort,
    SmtpPort,
    Insecure,
}

impl MailboxFormError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "Enter an email address.",
            Self::Password => "Enter the mailbox password.",
            Self::ImapHost => "Enter the IMAP server.",
            Self::SmtpHost => "Enter the SMTP server.",
            Self::ImapPort => "IMAP port must be 1–65535.",
            Self::SmtpPort => "SMTP port must be 1–65535.",
            Self::Insecure => "Unencrypted mail needs the warning box ticked.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddAccountError {
    Email,
    Duplicate,
}

impl AddAccountError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => MailboxFormError::Email.as_str(),
            Self::Duplicate => "That mailbox is already on this computer.",
        }
    }
}

/// Raw fields from the Add account form. Password is not kept after parse.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MailboxDraft<'a> {
    pub email: &'a str,
    pub password: &'a str,
    pub imap_host: &'a str,
    pub imap_port: &'a str,
    pub imap_security: MailSecurity,
    pub smtp_host: &'a str,
    pub smtp_port: &'a str,
    pub smtp_security: MailSecurity,
    pub allow_insecure: bool,
}

impl std::fmt::Debug for MailboxDraft<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailboxDraft")
            .field("email", &self.email)
            .field("password", &"[redacted]")
            .field("imap_host", &self.imap_host)
            .field("imap_port", &self.imap_port)
            .field("imap_security", &self.imap_security)
            .field("smtp_host", &self.smtp_host)
            .field("smtp_port", &self.smtp_port)
            .field("smtp_security", &self.smtp_security)
            .field("allow_insecure", &self.allow_insecure)
            .finish()
    }
}

/// Connection fields for Other IMAP. No secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxForm {
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: MailSecurity,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: MailSecurity,
}

impl MailboxForm {
    pub fn parse(draft: MailboxDraft<'_>) -> Result<Self, MailboxFormError> {
        let email = normalize_email(draft.email).ok_or(MailboxFormError::Email)?;
        if draft.password.is_empty() {
            return Err(MailboxFormError::Password);
        }
        let imap_host = normalize_host(draft.imap_host).ok_or(MailboxFormError::ImapHost)?;
        let smtp_host = normalize_host(draft.smtp_host).ok_or(MailboxFormError::SmtpHost)?;
        let imap_port = parse_port(draft.imap_port, draft.imap_security.default_imap_port())
            .ok_or(MailboxFormError::ImapPort)?;
        let smtp_port = parse_port(draft.smtp_port, draft.smtp_security.default_smtp_port())
            .ok_or(MailboxFormError::SmtpPort)?;
        if (draft.imap_security == MailSecurity::None || draft.smtp_security == MailSecurity::None)
            && !draft.allow_insecure
        {
            return Err(MailboxFormError::Insecure);
        }
        Ok(Self {
            email,
            imap_host,
            imap_port,
            imap_security: draft.imap_security,
            smtp_host,
            smtp_port,
            smtp_security: draft.smtp_security,
        })
    }

    pub fn account_id(&self) -> AccountId {
        account_id_from_email(&self.email)
    }

    pub fn display_name(&self) -> String {
        display_name_from_email(&self.email)
    }

    /// Microsoft 365 / Outlook.com IMAP/SMTP endpoints (D18, T-020).
    /// OAuth token is not on this type.
    pub fn microsoft(email: &str) -> Result<Self, MailboxFormError> {
        let email = normalize_email(email).ok_or(MailboxFormError::Email)?;
        Ok(Self {
            email,
            imap_host: "outlook.office365.com".into(),
            imap_port: 993,
            imap_security: MailSecurity::Ssl,
            smtp_host: "smtp.office365.com".into(),
            smtp_port: 587,
            smtp_security: MailSecurity::StartTls,
        })
    }

    /// Gmail IMAP/SMTP endpoints (D18, T-019). OAuth token is not on this type.
    pub fn gmail(email: &str) -> Result<Self, MailboxFormError> {
        let email = normalize_email(email).ok_or(MailboxFormError::Email)?;
        Ok(Self {
            email,
            imap_host: "imap.gmail.com".into(),
            imap_port: 993,
            imap_security: MailSecurity::Ssl,
            smtp_host: "smtp.gmail.com".into(),
            smtp_port: 587,
            smtp_security: MailSecurity::StartTls,
        })
    }
}

/// Partial edit for `Core::update_account` (T-021, §24). `None` fields keep
/// their current value. There is no `email` field: the address is the
/// account's identifier and is never editable here.
///
/// Setting any of the connection fields, or `new_password`, marks the edit
/// as touching the server connection — that requires `provider = "generic"`
/// and a successful probe before anything is written (see
/// [`Self::touches_connection`] and `Core::update_account`).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct AccountEdit {
    pub display_name: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_security: Option<MailSecurity>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<MailSecurity>,
    /// Present only when the user is changing (or re-confirming) the
    /// password; probed, never written to sqlite (D14).
    pub new_password: Option<String>,
}

impl AccountEdit {
    pub fn touches_connection(&self) -> bool {
        self.imap_host.is_some()
            || self.imap_port.is_some()
            || self.imap_security.is_some()
            || self.smtp_host.is_some()
            || self.smtp_port.is_some()
            || self.smtp_security.is_some()
            || self.new_password.is_some()
    }
}

impl std::fmt::Debug for AccountEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountEdit")
            .field("display_name", &self.display_name)
            .field("imap_host", &self.imap_host)
            .field("imap_port", &self.imap_port)
            .field("imap_security", &self.imap_security)
            .field("smtp_host", &self.smtp_host)
            .field("smtp_port", &self.smtp_port)
            .field("smtp_security", &self.smtp_security)
            .field(
                "new_password",
                &self.new_password.as_ref().map(|_| "[redacted]"),
            )
            .finish()
    }
}

pub fn unique_account_id<'a, I>(email: &str, taken: I) -> AccountId
where
    I: IntoIterator<Item = &'a str>,
{
    let taken: HashSet<&str> = taken.into_iter().collect();
    let base = account_id_from_email(email);
    if !taken.contains(base.as_str()) {
        return base;
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{}-{n}", base.as_str());
        if !taken.contains(candidate.as_str()) {
            return AccountId(candidate);
        }
        n += 1;
    }
}

pub fn account_id_from_email(email: &str) -> AccountId {
    let local = email.split('@').next().unwrap_or(email).trim();
    let mut id: String = local
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while id.contains("--") {
        id = id.replace("--", "-");
    }
    let id = id.trim_matches('-');
    AccountId(if id.is_empty() {
        "mailbox".into()
    } else {
        id.into()
    })
}

pub fn display_name_from_email(email: &str) -> String {
    let local = email.split('@').next().unwrap_or(email);
    let words: Vec<String> = local
        .split(['.', '_', '-', '+'])
        .filter(|s| !s.is_empty())
        .map(title_word)
        .collect();
    if words.is_empty() {
        "Mailbox".into()
    } else {
        words.join(" ")
    }
}

fn title_word(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn normalize_email(raw: &str) -> Option<String> {
    let email = raw.trim();
    let (local, domain) = email.split_once('@')?;
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return None;
    }
    Some(email.to_string())
}

/// Host of a From address, used as the Show-images allow key (T-117).
///
/// Image CDNs are shared across senders, so the allow-list is the mailbox
/// domain, not the `src` host. `Name <local@domain>` is fine; anything
/// that is not an address yields `None` and stays per-message.
pub fn sender_image_domain(raw: &str) -> Option<String> {
    let addr = mailbox_addr(raw)?;
    let host = addr.split_once('@')?.1;
    normalize_image_domain(host)
}

/// A stored allow-list entry. Same shape as the host [`sender_image_domain`]
/// returns, so a settings round-trip cannot grow a key the From parser
/// would not have produced.
pub fn normalize_image_domain(host: &str) -> Option<String> {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() || host.chars().count() > 253 {
        return None;
    }
    if host.starts_with('.') || host.ends_with('.') || host.contains("..") {
        return None;
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    {
        return None;
    }
    Some(host)
}

/// A To/Cc/Bcc field that can actually be delivered.
///
/// T-116: Compose used to treat any non-blank To as sendable, so `не-адрес`
/// was queued with "Message queued for sending". A token has to look like
/// `local@domain` or `Name <local@domain>`; comma/semicolon lists are fine;
/// an empty field is not.
pub fn recipient_field_is_sendable(raw: &str) -> bool {
    let mut any = false;
    for token in raw.split([',', ';']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if mailbox_addr(token).is_none() {
            return false;
        }
        any = true;
    }
    any
}

fn mailbox_addr(token: &str) -> Option<String> {
    let token = token.trim();
    let email = if let Some(start) = token.rfind('<') {
        if !token.ends_with('>') {
            return None;
        }
        token[start + 1..token.len() - 1].trim()
    } else {
        token
    };
    if email.chars().any(char::is_whitespace) {
        return None;
    }
    normalize_email(email)
}

pub(crate) fn normalize_host(raw: &str) -> Option<String> {
    let host = raw.trim();
    if host.is_empty() || host.contains(char::is_whitespace) {
        return None;
    }
    Some(host.to_string())
}

fn parse_port(raw: &str, default: u16) -> Option<u16> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Some(default);
    }
    let port: u16 = raw.parse().ok()?;
    (port > 0).then_some(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mail_security_parse_round_trips_as_str() {
        for sec in MailSecurity::ALL {
            assert_eq!(MailSecurity::parse(sec.as_str()), Some(sec));
        }
        assert_eq!(MailSecurity::parse("bogus"), None);
    }

    #[test]
    fn account_edit_debug_redacts_password() {
        let edit = AccountEdit {
            new_password: Some("hunter2".into()),
            ..Default::default()
        };
        let debug = format!("{edit:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn touches_connection_is_false_for_display_name_only() {
        let edit = AccountEdit {
            display_name: Some("New Name".into()),
            ..Default::default()
        };
        assert!(!edit.touches_connection());
        let edit = AccountEdit {
            imap_port: Some(993),
            ..Default::default()
        };
        assert!(edit.touches_connection());
    }

    #[allow(clippy::too_many_arguments)]
    fn draft<'a>(
        email: &'a str,
        password: &'a str,
        imap: &'a str,
        imap_port: &'a str,
        imap_sec: MailSecurity,
        smtp: &'a str,
        smtp_port: &'a str,
        smtp_sec: MailSecurity,
        allow: bool,
    ) -> MailboxDraft<'a> {
        MailboxDraft {
            email,
            password,
            imap_host: imap,
            imap_port,
            imap_security: imap_sec,
            smtp_host: smtp,
            smtp_port,
            smtp_security: smtp_sec,
            allow_insecure: allow,
        }
    }

    #[test]
    fn happy_path_fills_default_ports() {
        let form = MailboxForm::parse(draft(
            " you@example.com ",
            "secret",
            "imap.example.com",
            "",
            MailSecurity::Ssl,
            "smtp.example.com",
            "",
            MailSecurity::StartTls,
            false,
        ))
        .expect("form");
        assert_eq!(form.email, "you@example.com");
        assert_eq!(form.imap_port, 993);
        assert_eq!(form.smtp_port, 587);
        assert_eq!(form.account_id().as_str(), "you");
        assert_eq!(form.display_name(), "You");
        assert!(!format!("{form:?}").contains("secret"));
        let raw = draft(
            "you@example.com",
            "secret",
            "imap.example.com",
            "",
            MailSecurity::Ssl,
            "smtp.example.com",
            "",
            MailSecurity::StartTls,
            false,
        );
        let debug = format!("{raw:?}");
        assert!(!debug.contains("secret"), "{debug}");
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn gmail_form_uses_google_hosts() {
        let form = MailboxForm::gmail("you@gmail.com").unwrap();
        assert_eq!(form.imap_host, "imap.gmail.com");
        assert_eq!(form.imap_port, 993);
        assert_eq!(form.imap_security, MailSecurity::Ssl);
        assert_eq!(form.smtp_host, "smtp.gmail.com");
        assert_eq!(form.smtp_port, 587);
        assert_eq!(form.smtp_security, MailSecurity::StartTls);
        assert!(MailboxForm::gmail("not-an-email").is_err());
    }

    #[test]
    fn rejects_empty_password() {
        assert_eq!(
            MailboxForm::parse(draft(
                "a@b.com",
                "",
                "imap.b.com",
                "993",
                MailSecurity::Ssl,
                "smtp.b.com",
                "587",
                MailSecurity::StartTls,
                false,
            )),
            Err(MailboxFormError::Password)
        );
    }

    #[test]
    fn none_security_needs_checkbox() {
        assert_eq!(
            MailboxForm::parse(draft(
                "a@b.com",
                "x",
                "imap.b.com",
                "143",
                MailSecurity::None,
                "smtp.b.com",
                "25",
                MailSecurity::StartTls,
                false,
            )),
            Err(MailboxFormError::Insecure)
        );
        assert!(MailboxForm::parse(draft(
            "a@b.com",
            "x",
            "imap.b.com",
            "143",
            MailSecurity::None,
            "smtp.b.com",
            "25",
            MailSecurity::StartTls,
            true,
        ))
        .is_ok());
    }

    #[test]
    fn bad_email_and_host() {
        assert_eq!(
            MailboxForm::parse(draft(
                "nope",
                "x",
                "imap.b.com",
                "",
                MailSecurity::Ssl,
                "smtp.b.com",
                "",
                MailSecurity::StartTls,
                false,
            )),
            Err(MailboxFormError::Email)
        );
        assert_eq!(
            MailboxForm::parse(draft(
                "a@b.com",
                "x",
                "imap host",
                "",
                MailSecurity::Ssl,
                "smtp.b.com",
                "",
                MailSecurity::StartTls,
                false,
            )),
            Err(MailboxFormError::ImapHost)
        );
    }

    #[test]
    fn zero_port_is_invalid() {
        assert_eq!(
            MailboxForm::parse(draft(
                "a@b.com",
                "x",
                "imap.b.com",
                "0",
                MailSecurity::Ssl,
                "smtp.b.com",
                "587",
                MailSecurity::StartTls,
                false,
            )),
            Err(MailboxFormError::ImapPort)
        );
    }

    #[test]
    fn sender_image_domain_is_the_host_of_from() {
        assert_eq!(
            sender_image_domain("jane@example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            sender_image_domain("Jane Roe <jane@News.Example.COM>").as_deref(),
            Some("news.example.com")
        );
        assert_eq!(sender_image_domain("не-адрес"), None);
        assert_eq!(sender_image_domain(""), None);
        assert_eq!(
            normalize_image_domain("Example.COM").as_deref(),
            Some("example.com")
        );
        assert_eq!(normalize_image_domain("bad host"), None);
        assert_eq!(normalize_image_domain(".example.com"), None);
    }

    #[test]
    fn a_to_field_without_an_at_sign_is_not_sendable() {
        assert!(!recipient_field_is_sendable(""));
        assert!(!recipient_field_is_sendable("   "));
        assert!(!recipient_field_is_sendable("не-адрес"));
        assert!(!recipient_field_is_sendable("jane"));
        assert!(!recipient_field_is_sendable("jane@example.com, не-адрес"));
        assert!(recipient_field_is_sendable("jane@example.com"));
        assert!(recipient_field_is_sendable("Jane Roe <jane@example.com>"));
        assert!(recipient_field_is_sendable("a@b.com, c@d.com"));
        assert!(recipient_field_is_sendable(" a@b.com ; c@d.com "));
    }

    #[test]
    fn john_doe_display_and_id() {
        assert_eq!(
            account_id_from_email("john.doe@example.com").as_str(),
            "john.doe"
        );
        assert_eq!(display_name_from_email("john.doe@example.com"), "John Doe");
    }

    #[test]
    fn unique_id_avoids_taken() {
        assert_eq!(
            unique_account_id("john@elsewhere.test", ["john", "jane"].into_iter()).as_str(),
            "john-2"
        );
    }
}
