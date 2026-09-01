//! Generic IMAP + SMTP connect (T-018). LOGIN / STARTTLS / SSL / none.

use std::io::{Read, Write};

use feathermail_core::{ConnectError, ConnectOk, MailConnector, MailSecurity, MailboxForm};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::SmtpTransport;

use crate::wire::{
    capability, expect_greeting, imap_connect, looks_like_auth, net, read_tagged, sanitize,
    smtp_err, tagged, unreachable, MailLeg, TIMEOUT,
};

/// D18: generic IMAP/SMTP. Gmail is [`crate::GmailImap`].
#[derive(Clone, Debug, Default)]
pub struct GenericImapSmtp;

impl MailConnector for GenericImapSmtp {
    fn probe(&self, form: &MailboxForm, password: &str) -> Result<ConnectOk, ConnectError> {
        if password.is_empty() {
            return Err(ConnectError::invalid("Enter the mailbox password."));
        }
        let capabilities = imap_probe(form, password)?;
        smtp_probe(form, password)?;
        Ok(ConnectOk { capabilities })
    }
}

fn imap_probe(form: &MailboxForm, password: &str) -> Result<Vec<String>, ConnectError> {
    let mut conn = imap_connect(form)?;
    if conn.read_greeting {
        expect_greeting(&mut conn.stream)?;
    }
    login(&mut conn.stream, &form.email, password)?;
    let caps = capability(&mut conn.stream, "A3")?;
    let _ = tagged(&mut conn.stream, "A4", "LOGOUT");
    Ok(caps)
}

fn smtp_probe(form: &MailboxForm, password: &str) -> Result<(), ConnectError> {
    // Unauthenticated NOOP is enough for sinks like Mailhog. AUTH only if required.
    if matches!(smtp_transport(form, None)?.test_connection(), Ok(true)) {
        return Ok(());
    }
    match smtp_transport(form, Some(password))?.test_connection() {
        Ok(true) => Ok(()),
        // T-103: both arms used to collapse into "Couldn't reach the server",
        // which is the same sentence the IMAP half produces -- the owner had
        // no way to tell that the mailbox answered and only submission did
        // not. See `wire::unreachable`.
        Ok(false) => Err(unreachable(
            MailLeg::Smtp,
            &form.smtp_host,
            form.smtp_port,
            "SMTP NOOP failed",
        )),
        Err(err) => {
            let text = sanitize(&err.to_string());
            if looks_like_auth(&text) {
                Err(ConnectError::auth(text))
            } else {
                Err(unreachable(
                    MailLeg::Smtp,
                    &form.smtp_host,
                    form.smtp_port,
                    text,
                ))
            }
        }
    }
}

fn smtp_transport(
    form: &MailboxForm,
    password: Option<&str>,
) -> Result<SmtpTransport, ConnectError> {
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
    builder = builder.timeout(Some(TIMEOUT));
    if let Some(password) = password {
        builder = builder.credentials(Credentials::new(form.email.clone(), password.to_string()));
    }
    Ok(builder.build())
}

fn login<S: Read + Write>(stream: &mut S, user: &str, password: &str) -> Result<(), ConnectError> {
    let user = imap_quote(user)?;
    let pass = imap_quote(password)?;
    let mut cmd = Vec::with_capacity(16 + user.len() + pass.len());
    cmd.extend_from_slice(b"A2 LOGIN ");
    cmd.extend_from_slice(user.as_bytes());
    cmd.push(b' ');
    cmd.extend_from_slice(pass.as_bytes());
    cmd.extend_from_slice(b"\r\n");
    stream.write_all(&cmd).map_err(net)?;
    stream.flush().map_err(net)?;
    cmd.fill(0);
    let lines = read_tagged(stream, "A2")?;
    let last = lines.last().cloned().unwrap_or_default();
    if last.starts_with("A2 OK") {
        Ok(())
    } else {
        Err(ConnectError::auth(sanitize(&last)))
    }
}

fn imap_quote(s: &str) -> Result<String, ConnectError> {
    if s.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n') {
        return Err(ConnectError::invalid("That mailbox value isn't valid."));
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    fn spawn_imap(accept_password: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK Dovecot ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let upper = line.to_ascii_uppercase();
                if upper.contains(" LOGIN ") {
                    let ok = line.contains(accept_password);
                    let tag = line.split_whitespace().next().unwrap_or("A2");
                    if ok {
                        write!(writer, "{tag} OK logged in\r\n").unwrap();
                    } else {
                        write!(
                            writer,
                            "{tag} NO [AUTHENTICATIONFAILED] Authentication failed\r\n"
                        )
                        .unwrap();
                    }
                } else if upper.contains(" CAPABILITY") {
                    let tag = line.split_whitespace().next().unwrap_or("A3");
                    write!(writer, "* CAPABILITY IMAP4rev1 STARTTLS AUTH=PLAIN\r\n").unwrap();
                    write!(writer, "{tag} OK CAPABILITY completed\r\n").unwrap();
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

    fn spawn_smtp() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "220 mailhog\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let upper = line.to_ascii_uppercase();
                if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                    write!(writer, "250-hello\r\n250-AUTH PLAIN LOGIN\r\n250 OK\r\n").unwrap();
                } else if upper.starts_with("NOOP") {
                    write!(writer, "250 OK\r\n").unwrap();
                } else if upper.starts_with("QUIT") {
                    write!(writer, "221 bye\r\n").unwrap();
                    break;
                } else if upper.starts_with("AUTH") {
                    write!(writer, "235 2.7.0 Authentication successful\r\n").unwrap();
                } else {
                    write!(writer, "250 OK\r\n").unwrap();
                }
                writer.flush().unwrap();
            }
        });
        port
    }

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

    /// T-103 (владелец: «нажал добавить аккаунт, ввёл gmail-почту, спиннер и
    /// couldn't reach the server»): the mailbox answered, submission did not,
    /// and one sentence covered both. Now the message names the leg and the
    /// endpoint, so the owner can see it is the outgoing port that is dead --
    /// which on that network is exactly what 465 to Google is.
    ///
    /// A port nobody listens on is the deterministic stand-in for a blocked
    /// route: same class of failure, no timeout in the test.
    #[test]
    fn an_unreachable_smtp_port_says_so_by_name() {
        let imap = spawn_imap("correct-horse");
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        thread::sleep(Duration::from_millis(30));
        let err = GenericImapSmtp
            .probe(&form(imap, dead), "correct-horse")
            .unwrap_err();
        let ConnectError::Network { message, .. } = &err else {
            panic!("a closed port is a network failure, got {err:?}");
        };
        assert!(
            message.contains("outgoing mail (SMTP)")
                && message.contains(&format!("127.0.0.1:{dead}")),
            "the message has to name the leg and the endpoint, got {message:?}"
        );
        assert!(
            !message.contains("incoming"),
            "IMAP answered -- saying otherwise sends the owner after the wrong setting"
        );
    }

    /// The other half of the same complaint: a dead IMAP endpoint must not
    /// blame the outgoing server.
    #[test]
    fn an_unreachable_imap_port_says_so_by_name() {
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        let err = GenericImapSmtp
            .probe(&form(dead, dead), "correct-horse")
            .unwrap_err();
        let ConnectError::Network { message, .. } = &err else {
            panic!("a closed port is a network failure, got {err:?}");
        };
        assert!(
            message.contains("incoming mail (IMAP)")
                && message.contains(&format!("127.0.0.1:{dead}")),
            "got {message:?}"
        );
    }

    #[test]
    fn login_ok_and_capabilities() {
        let imap = spawn_imap("correct-horse");
        let smtp = spawn_smtp();
        thread::sleep(Duration::from_millis(30));
        let ok = GenericImapSmtp
            .probe(&form(imap, smtp), "correct-horse")
            .unwrap();
        assert!(ok.capabilities.iter().any(|c| c == "IMAP4rev1"));
    }

    #[test]
    fn wrong_password_is_human() {
        let imap = spawn_imap("correct-horse");
        let smtp = spawn_smtp();
        thread::sleep(Duration::from_millis(30));
        let err = GenericImapSmtp
            .probe(&form(imap, smtp), "wrong")
            .unwrap_err();
        match err {
            ConnectError::Auth { message, details } => {
                assert_eq!(message, "That password wasn't accepted.");
                assert!(!message.to_ascii_lowercase().contains("imap"));
                let details = details.unwrap_or_default();
                assert!(!details.contains("wrong"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn live_dovecot_mailhog_if_running() {
        let imap_host =
            std::env::var("FEATHERMAIL_TEST_IMAP_HOST").unwrap_or_else(|_| "127.0.0.1".into());
        let imap_port: u16 = std::env::var("FEATHERMAIL_TEST_IMAP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1143);
        let smtp_port: u16 = std::env::var("FEATHERMAIL_TEST_SMTP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1025);
        let addr = format!("{imap_host}:{imap_port}");
        if TcpStream::connect_timeout(
            &addr
                .parse()
                .unwrap_or_else(|_| "127.0.0.1:1143".parse().unwrap()),
            Duration::from_millis(400),
        )
        .is_err()
        {
            eprintln!("skip live Dovecot/Mailhog (docker not up on {addr})");
            return;
        }
        let user =
            std::env::var("FEATHERMAIL_TEST_USER").unwrap_or_else(|_| "you@example.com".into());
        let pass =
            std::env::var("FEATHERMAIL_TEST_PASS").unwrap_or_else(|_| "correct-horse".into());
        let form = MailboxForm {
            email: user,
            imap_host: imap_host.clone(),
            imap_port,
            imap_security: MailSecurity::None,
            smtp_host: imap_host,
            smtp_port,
            smtp_security: MailSecurity::None,
        };
        GenericImapSmtp
            .probe(&form, &pass)
            .expect("Dovecot LOGIN + Mailhog SMTP");
        let err = GenericImapSmtp
            .probe(&form, "not-the-password")
            .unwrap_err();
        assert!(
            matches!(err, ConnectError::Auth { .. }),
            "wrong password must be AUTH, got {err:?}"
        );
    }
}
