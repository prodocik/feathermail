//! Shared XOAUTH2 IMAP/SMTP connector wire protocol (T-019, T-020, D18).
//!
//! `GmailImap` (`gmail.rs`) and `MicrosoftImap` (`microsoft.rs`) are both
//! IMAP `AUTHENTICATE XOAUTH2` + `CAPABILITY` + `LOGOUT`, then SMTP XOAUTH2
//! via `lettre` — the wire handshake does not differ between the two
//! providers, only the endpoints in `MailboxForm` and the marker type do.
//! This module holds that handshake exactly once.

use std::io::{Read, Write};

use feathermail_core::{ConnectError, ConnectOk, MailSecurity, MailboxForm};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::SmtpTransport;

use crate::wire::{
    capability, expect_greeting, imap_connect, looks_like_auth, sanitize, smtp_err, tagged, TIMEOUT,
};

/// Probe IMAP + SMTP over XOAUTH2. Empty token → reauth without a round trip.
pub(crate) fn probe_xoauth2(
    form: &MailboxForm,
    access_token: &str,
) -> Result<ConnectOk, ConnectError> {
    if access_token.is_empty() {
        return Err(ConnectError::reauth("missing_access_token"));
    }
    let capabilities = imap_xoauth2(form, access_token)?;
    smtp_xoauth2(form, access_token)?;
    Ok(ConnectOk { capabilities })
}

fn imap_xoauth2(form: &MailboxForm, access_token: &str) -> Result<Vec<String>, ConnectError> {
    let mut conn = imap_connect(form)?;
    if conn.read_greeting {
        expect_greeting(&mut conn.stream)?;
    }
    authenticate_xoauth2(&mut conn.stream, &form.email, access_token)?;
    let caps = capability(&mut conn.stream, "A3")?;
    let _ = tagged(&mut conn.stream, "A4", "LOGOUT");
    Ok(caps)
}

/// T-165: one implementation of the XOAUTH2 bind, shared with the live
/// session path (`session.rs`). It used to be duplicated here with the
/// same single-line read, so the bug `wire::read_authenticate_reply`
/// documents existed twice and could be fixed in only one of them.
fn authenticate_xoauth2<S: Read + Write>(
    stream: &mut S,
    user: &str,
    access_token: &str,
) -> Result<(), ConnectError> {
    crate::wire::imap_authenticate_xoauth2(stream, "A2", user, access_token)
}

fn smtp_xoauth2(form: &MailboxForm, access_token: &str) -> Result<(), ConnectError> {
    let mut builder = match form.smtp_security {
        MailSecurity::Ssl => {
            let params = TlsParameters::new(form.smtp_host.clone()).map_err(smtp_err)?;
            SmtpTransport::relay(&form.smtp_host)
                .map_err(smtp_err)?
                .port(form.smtp_port)
                .tls(Tls::Wrapper(params))
        }
        MailSecurity::StartTls => SmtpTransport::starttls_relay(&form.smtp_host)
            .map_err(smtp_err)?
            .port(form.smtp_port),
        MailSecurity::None => SmtpTransport::builder_dangerous(&form.smtp_host)
            .port(form.smtp_port)
            .tls(Tls::None),
    };
    builder = builder
        .timeout(Some(TIMEOUT))
        .credentials(Credentials::new(
            form.email.clone(),
            access_token.to_string(),
        ))
        .authentication(vec![Mechanism::Xoauth2]);
    match builder.build().test_connection() {
        Ok(true) => Ok(()),
        Ok(false) => Err(ConnectError::network("SMTP NOOP failed")),
        Err(err) => {
            let text = sanitize(&err.to_string());
            if looks_like_auth(&text) {
                Err(ConnectError::reauth(text))
            } else {
                Err(ConnectError::network(text))
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn plaintext_form(email: &str, imap_port: u16, smtp_port: u16) -> MailboxForm {
    MailboxForm {
        email: email.into(),
        imap_host: "127.0.0.1".into(),
        imap_port,
        imap_security: MailSecurity::None,
        smtp_host: "127.0.0.1".into(),
        smtp_port,
        smtp_security: MailSecurity::None,
    }
}

/// Fake local IMAP/SMTP servers for `gmail.rs` and `microsoft.rs` probe
/// tests. Neither provider's tests assert the exact greeting banner, so one
/// pair of fakes covers both.
#[cfg(test)]
pub(crate) mod fake_server {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    pub(crate) fn spawn_imap(accept_token: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK IMAP4rev1 Service Ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let upper = line.to_ascii_uppercase();
                if upper.contains("AUTHENTICATE XOAUTH2") {
                    let tag = line.split_whitespace().next().unwrap_or("A2");
                    let payload = line.split_whitespace().nth(3).unwrap_or("");
                    let decoded = base64::Engine::decode(
                        &base64::engine::general_purpose::STANDARD,
                        payload.trim(),
                    )
                    .unwrap_or_default();
                    let text = String::from_utf8_lossy(&decoded);
                    let ok = text.contains(accept_token) && text.contains("user=");
                    if ok {
                        // T-165: real Gmail sends its post-login capability
                        // list *before* the tagged OK. The fake said only
                        // "{tag} OK", which is why a reader that stopped at
                        // the first line passed every test here and failed
                        // against Google on its own success line.
                        write!(
                            writer,
                            "* CAPABILITY IMAP4rev1 UNSELECT IDLE NAMESPACE QUOTA ID XLIST \
                             CHILDREN X-GM-EXT-1 UIDPLUS COMPRESS=DEFLATE ENABLE MOVE \
                             CONDSTORE ESEARCH UTF8=ACCEPT\r\n{tag} OK Success\r\n"
                        )
                        .unwrap();
                    } else {
                        write!(
                            writer,
                            "+ eyJzdGF0dXMiOiI0MDEiLCJzY2hlbWVzIjoiQmVhcmVyIn0=\r\n"
                        )
                        .unwrap();
                        writer.flush().unwrap();
                        let mut extra = String::new();
                        let _ = reader.read_line(&mut extra);
                        write!(
                            writer,
                            "{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n"
                        )
                        .unwrap();
                    }
                } else if upper.contains(" CAPABILITY") {
                    let tag = line.split_whitespace().next().unwrap_or("A3");
                    write!(
                        writer,
                        "* CAPABILITY IMAP4rev1 AUTH=XOAUTH2\r\n{tag} OK CAPABILITY completed\r\n"
                    )
                    .unwrap();
                } else if upper.contains(" LOGOUT") {
                    let tag = line.split_whitespace().next().unwrap_or("A4");
                    write!(writer, "* BYE\r\n{tag} OK LOGOUT\r\n").unwrap();
                    break;
                } else {
                    let tag = line.split_whitespace().next().unwrap_or("*");
                    write!(writer, "{tag} BAD\r\n").unwrap();
                }
                writer.flush().unwrap();
            }
        });
        port
    }

    pub(crate) fn spawn_smtp() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "220 fake.smtp.test\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let upper = line.to_ascii_uppercase();
                if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                    write!(writer, "250-hello\r\n250-AUTH XOAUTH2\r\n250 OK\r\n").unwrap();
                } else if upper.starts_with("AUTH") {
                    write!(writer, "235 2.7.0 Accepted\r\n").unwrap();
                } else if upper.starts_with("NOOP") {
                    write!(writer, "250 OK\r\n").unwrap();
                } else if upper.starts_with("QUIT") {
                    write!(writer, "221 bye\r\n").unwrap();
                    break;
                } else {
                    write!(writer, "250 OK\r\n").unwrap();
                }
                writer.flush().unwrap();
            }
        });
        port
    }
}

#[cfg(test)]
mod tests {
    use super::fake_server::{spawn_imap, spawn_smtp};
    use super::*;
    use feathermail_core::ErrorCode;
    use std::thread;
    use std::time::Duration;

    fn form(imap_port: u16, smtp_port: u16) -> MailboxForm {
        MailboxForm {
            email: "you@example.com".into(),
            imap_host: "127.0.0.1".into(),
            imap_port,
            imap_security: MailSecurity::None,
            smtp_host: "127.0.0.1".into(),
            smtp_port,
            smtp_security: MailSecurity::None,
        }
    }

    #[test]
    fn probe_xoauth2_ok_and_capabilities() {
        let imap = spawn_imap("token.good");
        let smtp = spawn_smtp();
        thread::sleep(Duration::from_millis(30));
        let ok = probe_xoauth2(&form(imap, smtp), "token.good").unwrap();
        assert!(ok.capabilities.iter().any(|c| c == "AUTH=XOAUTH2"));
    }

    #[test]
    fn probe_xoauth2_revoked_token_is_human() {
        let imap = spawn_imap("token.good");
        let smtp = spawn_smtp();
        thread::sleep(Duration::from_millis(30));
        let err = probe_xoauth2(&form(imap, smtp), "token.revoked").unwrap_err();
        match err {
            ConnectError::Auth { message, details } => {
                assert_eq!(message, ErrorCode::AuthRequired.default_message());
                assert!(!message.to_ascii_lowercase().contains("xoauth"));
                assert!(!message.to_ascii_lowercase().contains("imap"));
                let details = details.unwrap_or_default();
                assert!(!details.contains("token.revoked"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn empty_access_token_is_auth_error_without_network() {
        // Ports are not even listened on: an empty token must fail before
        // any IMAP/SMTP round trip.
        let err = probe_xoauth2(&form(1, 1), "").unwrap_err();
        match err {
            ConnectError::Auth { message, details } => {
                assert_eq!(message, ErrorCode::AuthRequired.default_message());
                assert_eq!(details.as_deref(), Some("missing_access_token"));
            }
            other => panic!("{other:?}"),
        }
    }
}
