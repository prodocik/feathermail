//! Gmail IMAP/SMTP with XOAUTH2 (T-019, D18). Label mapping is T-022/T-029.
//!
//! The XOAUTH2 wire handshake itself is shared with Microsoft (T-020) in
//! `xoauth2.rs`; this file only carries the Gmail marker type.

use feathermail_core::{ConnectError, ConnectOk, MailConnector, MailboxForm};

use crate::xoauth2::probe_xoauth2;

/// D18: Gmail via IMAP XOAUTH2. Microsoft is T-020 (`MicrosoftImap`).
#[derive(Clone, Debug, Default)]
pub struct GmailImap;

impl MailConnector for GmailImap {
    fn probe(&self, form: &MailboxForm, access_token: &str) -> Result<ConnectOk, ConnectError> {
        probe_xoauth2(form, access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xoauth2::fake_server::{spawn_imap, spawn_smtp};
    use feathermail_core::ErrorCode;
    use std::thread;
    use std::time::Duration;

    fn form(imap_port: u16, smtp_port: u16) -> MailboxForm {
        crate::xoauth2::plaintext_form("you@gmail.com", imap_port, smtp_port)
    }

    #[test]
    fn xoauth2_ok_and_capabilities() {
        let imap = spawn_imap("ya29.good");
        let smtp = spawn_smtp();
        thread::sleep(Duration::from_millis(30));
        let ok = GmailImap.probe(&form(imap, smtp), "ya29.good").unwrap();
        assert!(ok.capabilities.iter().any(|c| c == "AUTH=XOAUTH2"));
    }

    #[test]
    fn revoked_token_is_human() {
        let imap = spawn_imap("ya29.good");
        let smtp = spawn_smtp();
        thread::sleep(Duration::from_millis(30));
        let err = GmailImap
            .probe(&form(imap, smtp), "ya29.revoked")
            .unwrap_err();
        match err {
            ConnectError::Auth { message, details } => {
                assert_eq!(message, ErrorCode::AuthRequired.default_message());
                assert!(!message.to_ascii_lowercase().contains("xoauth"));
                assert!(!message.to_ascii_lowercase().contains("imap"));
                let details = details.unwrap_or_default();
                assert!(!details.contains("ya29.revoked"));
            }
            other => panic!("{other:?}"),
        }
    }
}
