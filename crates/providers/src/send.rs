//! SMTP delivery for durable Core outbox snapshots (T-045).

use std::str::FromStr;

use feathermail_core::{ApplyError, Draft, MailSecurity, MailboxForm, OutboxMessage};
use lettre::address::Envelope;
use lettre::message::{
    header::{ContentType, To},
    Attachment, Mailbox, MultiPart, SinglePart,
};
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{Message, SmtpTransport, Transport};

use crate::wire::{looks_like_auth, sanitize, TIMEOUT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtpAuth {
    Password,
    Xoauth2,
}

/// Sends one frozen outbox row. The secret is borrowed only while building
/// the transport and is never stored in the message or returned in errors.
pub fn send_outbox(
    form: &MailboxForm,
    auth: SmtpAuth,
    secret: &str,
    outgoing: &OutboxMessage,
) -> Result<(), ApplyError> {
    let message = build_outbox_message(outgoing)?;
    send_message(form, auth, secret, &message)
}

pub fn build_outbox_message(outgoing: &OutboxMessage) -> Result<Message, ApplyError> {
    let mut builder = Message::builder()
        .from(parse_mailbox(&outgoing.from)?)
        .subject(&outgoing.subject);
    let recipients = parse_recipients(&outgoing.to)?;
    if recipients.is_empty() {
        return Err(ApplyError::Unsupported);
    }
    for recipient in recipients {
        builder = builder.to(recipient);
    }
    for recipient in parse_recipients(&outgoing.cc)? {
        builder = builder.cc(recipient);
    }
    for recipient in parse_recipients(&outgoing.bcc)? {
        builder = builder.bcc(recipient);
    }
    if let Some(id) = outgoing
        .in_reply_to
        .as_ref()
        .filter(|id| !id.trim().is_empty())
    {
        builder = builder.in_reply_to(id.clone());
    }
    if let Some(ids) = outgoing
        .references
        .as_ref()
        .filter(|ids| !ids.trim().is_empty())
    {
        builder = builder.references(ids.clone());
    }
    let message = if outgoing.attachments.is_empty() {
        builder
            .body(outgoing.body.clone())
            .map_err(|_| ApplyError::Unsupported)?
    } else {
        let mut mixed = MultiPart::mixed().singlepart(SinglePart::plain(outgoing.body.clone()));
        for attachment in &outgoing.attachments {
            let bytes = std::fs::read(&attachment.source_path).map_err(|_| ApplyError::NotFound)?;
            let content_type = ContentType::parse(&attachment.mime)
                .unwrap_or(ContentType::parse("application/octet-stream").unwrap());
            mixed = mixed
                .singlepart(Attachment::new(attachment.filename.clone()).body(bytes, content_type));
        }
        builder
            .multipart(mixed)
            .map_err(|_| ApplyError::Unsupported)?
    };
    Ok(message)
}

/// Formats a local compose draft for IMAP `APPEND` into the server's Drafts
/// mailbox (T-042). Unlike an outgoing message, a draft may legitimately
/// have no recipients yet; all editable recipient fields are preserved when
/// present. Attachments intentionally stay out of this helper until T-044
/// owns their streaming lifecycle.
pub fn build_draft_message(draft: &Draft) -> Result<Message, ApplyError> {
    let from = parse_mailbox(&draft.from)?;
    let recipients = parse_recipients(&draft.to)?;
    let cc = parse_recipients(&draft.cc)?;
    let bcc = parse_recipients(&draft.bcc)?;
    let needs_placeholder_recipient = recipients.is_empty() && cc.is_empty() && bcc.is_empty();

    // lettre derives an SMTP envelope while it formats a message and rejects
    // one with no recipients. IMAP APPEND has no envelope, however, and an
    // unfinished compose draft is valid with every recipient field empty.
    // Feed a temporary local recipient to lettre solely to use its safe header
    // and body encoding, then remove that synthetic header before APPEND.
    let mut builder = Message::builder()
        .from(from.clone())
        .subject(&draft.subject);
    if needs_placeholder_recipient {
        builder = builder.to(from);
    }
    for recipient in recipients {
        builder = builder.to(recipient);
    }
    for recipient in cc {
        builder = builder.cc(recipient);
    }
    for recipient in bcc {
        builder = builder.bcc(recipient);
    }
    let mut message = builder
        // A draft is server-side editable state, not an SMTP delivery. Keep
        // Bcc so reopening it does not silently discard a recipient.
        .keep_bcc()
        .body(draft.body.clone())
        .map_err(|_| ApplyError::Unsupported)?;
    if needs_placeholder_recipient {
        message.headers_mut().remove::<To>();
    }
    Ok(message)
}

pub fn send_message(
    form: &MailboxForm,
    auth: SmtpAuth,
    secret: &str,
    message: &Message,
) -> Result<(), ApplyError> {
    let transport = smtp_transport(form, auth, secret)?;
    transport.send(message).map_err(classify_smtp_error)?;
    Ok(())
}

/// Sends a message that is already encoded (T-113). `Transport::send` would
/// call `Message::formatted()` itself, and the caller needs those same bytes
/// again for the Sent `APPEND` -- with a 100 MB attachment (the largest
/// `Core` accepts) each encoded copy is 136 MiB, so formatting twice cost a
/// measured 420 MiB peak against D4's 350 MB ceiling. One encode, two uses.
pub fn send_formatted(
    form: &MailboxForm,
    auth: SmtpAuth,
    secret: &str,
    envelope: &Envelope,
    raw: &[u8],
) -> Result<(), ApplyError> {
    let transport = smtp_transport(form, auth, secret)?;
    transport
        .send_raw(envelope, raw)
        .map_err(classify_smtp_error)?;
    Ok(())
}

fn parse_recipients(value: &str) -> Result<Vec<Mailbox>, ApplyError> {
    value
        .split([',', ';'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(parse_mailbox)
        .collect()
}

fn parse_mailbox(value: &str) -> Result<Mailbox, ApplyError> {
    Mailbox::from_str(value.trim()).map_err(|_| ApplyError::Unsupported)
}

fn smtp_transport(
    form: &MailboxForm,
    auth: SmtpAuth,
    secret: &str,
) -> Result<SmtpTransport, ApplyError> {
    let mut builder = match form.smtp_security {
        MailSecurity::Ssl => {
            let params =
                TlsParameters::new(form.smtp_host.clone()).map_err(|_| ApplyError::Unsupported)?;
            SmtpTransport::relay(&form.smtp_host)
                .map_err(|_| ApplyError::Unsupported)?
                .port(form.smtp_port)
                .tls(Tls::Wrapper(params))
        }
        MailSecurity::StartTls => SmtpTransport::starttls_relay(&form.smtp_host)
            .map_err(|_| ApplyError::Unsupported)?
            .port(form.smtp_port),
        MailSecurity::None => SmtpTransport::builder_dangerous(&form.smtp_host)
            .port(form.smtp_port)
            .tls(Tls::None),
    };
    builder = builder
        .timeout(Some(TIMEOUT))
        .credentials(Credentials::new(form.email.clone(), secret.to_string()));
    if auth == SmtpAuth::Xoauth2 {
        builder = builder.authentication(vec![Mechanism::Xoauth2]);
    }
    Ok(builder.build())
}

fn classify_smtp_error(err: lettre::transport::smtp::Error) -> ApplyError {
    let text = sanitize(&err.to_string());
    if looks_like_auth(&text) {
        ApplyError::Auth
    } else if err.is_timeout() || err.is_client() || err.is_response() || err.is_transient() {
        ApplyError::Network
    } else {
        ApplyError::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_core::{AccountId, DraftId};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn outgoing() -> OutboxMessage {
        OutboxMessage {
            id: "o1".into(),
            account_id: AccountId("john".into()),
            draft_id: Some(DraftId("d1".into())),
            from: "John <john@example.com>".into(),
            to: "jane@example.com, Other <other@example.com>".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Hello".into(),
            body: "Private body".into(),
            in_reply_to: Some("<parent@example.com>".into()),
            references: Some("<root@example.com> <parent@example.com>".into()),
            attachments: Vec::new(),
            status: "queued".into(),
        }
    }

    /// Peak resident set of this process, in bytes, from `VmHWM` -- the
    /// kernel's own high-water mark, so a peak that has already been freed
    /// still counts. Linux only, which is the only platform v0.1 ships to.
    fn peak_rss_bytes() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status
            .lines()
            .find(|line| line.starts_with("VmHWM:"))
            .expect("VmHWM must be present on Linux");
        let kib: u64 = line
            .split_whitespace()
            .nth(1)
            .and_then(|n| n.parse().ok())
            .unwrap();
        kib * 1024
    }

    /// T-070's "100 MB outgoing attachment ... without a 100 MB RSS jump",
    /// measured rather than assumed (T-113). `MAX_OUTGOING_ATTACHMENT_BYTES`
    /// is exactly 100 MB, so this is the worst case a person can actually
    /// compose, not a synthetic one.
    ///
    /// Ignored by default: it writes and encodes 100 MB, which has no place
    /// in an ordinary `cargo test` run. Run it with
    /// `cargo test -p feathermail-providers --lib -- --ignored --nocapture`
    /// and read the printed numbers; `docs/plan.md` T-113 records what this
    /// machine reported and what follows from it.
    #[test]
    #[ignore = "writes and encodes 100 MB; run explicitly with --ignored"]
    fn a_hundred_megabyte_outgoing_attachment_costs_this_much_memory() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("big.bin");
        let size = 100 * 1024 * 1024usize;
        {
            let mut file = std::fs::File::create(&source).unwrap();
            // Real bytes, not ASCII filler: lettre picks the transfer
            // encoding from the content, and a file of `A`s would ride as
            // 7bit and understate the cost of the base64 an actual
            // attachment forces.
            let chunk: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
            for _ in 0..(size / chunk.len()) {
                file.write_all(&chunk).unwrap();
            }
            file.flush().unwrap();
        }

        let before = peak_rss_bytes();
        let mut outgoing = outgoing();
        outgoing.attachments = vec![feathermail_core::OutgoingAttachment {
            filename: "big.bin".into(),
            mime: "application/octet-stream".into(),
            size_bytes: size as u64,
            source_path: source.clone(),
        }];

        let message = build_outbox_message(&outgoing).unwrap();
        let after_build = peak_rss_bytes();
        // What `SendingSession` does next for the Sent APPEND, and what
        // lettre's own SMTP transport does internally to produce the DATA
        // payload: one more full copy of the encoded message.
        let formatted = message.formatted();
        let after_format = peak_rss_bytes();
        assert!(formatted.len() > size);
        // What `SendingSession::apply` does now: drop the `Message` as soon
        // as the encoded bytes exist, then hand the same buffer to SMTP
        // `DATA` and to the Sent `APPEND`. The old shape encoded a second
        // copy for the APPEND and peaked at 420 MiB on this machine.
        drop(message);
        let after_drop = peak_rss_bytes();

        eprintln!(
            "100 MB outgoing attachment: peak RSS before {} MiB, after build {} MiB, \
             after formatted() {} MiB, after dropping the message {} MiB \
             (encoded message {} MiB)",
            before / (1024 * 1024),
            after_build / (1024 * 1024),
            after_format / (1024 * 1024),
            after_drop / (1024 * 1024),
            formatted.len() / (1024 * 1024),
        );
        // The peak is the encode itself and cannot be lower without a
        // streaming SMTP client of our own (see `docs/plan.md` T-113). What
        // must not come back is the second encode on top of it.
        assert!(
            after_format < 350 * 1024 * 1024,
            "sending the largest attachment Core accepts must stay under D4's \
             350 MB ceiling; peaked at {} MiB",
            after_format / (1024 * 1024)
        );
    }

    #[test]
    fn recipient_parser_is_local_and_accepts_display_names() {
        assert_eq!(parse_recipients(&outgoing().to).unwrap().len(), 2);
        assert!(parse_recipients("not an address").is_err());
    }

    #[test]
    fn outbox_debug_never_contains_body() {
        assert!(!format!("{:?}", outgoing()).contains("Private body"));
    }

    #[test]
    fn draft_message_allows_an_unaddressed_saved_draft() {
        let draft = Draft {
            id: DraftId("d1".into()),
            account_id: AccountId("john".into()),
            thread_id: None,
            in_reply_to: None,
            from: "John <john@example.com>".into(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Unfinished".into(),
            body: "Private draft body".into(),
            updated_at: 1,
            remote_uid: None,
        };
        let raw = String::from_utf8(build_draft_message(&draft).unwrap().formatted()).unwrap();
        assert!(raw.contains("Subject: Unfinished"));
        assert!(raw.contains("Private draft body"));
        assert!(!raw.contains("\r\nTo:"));
    }

    #[test]
    fn draft_message_keeps_bcc_for_server_side_editing() {
        let draft = Draft {
            id: DraftId("d1".into()),
            account_id: AccountId("john".into()),
            thread_id: None,
            in_reply_to: None,
            from: "John <john@example.com>".into(),
            to: String::new(),
            cc: String::new(),
            bcc: "blind@example.com".into(),
            subject: "Private recipients".into(),
            body: String::new(),
            updated_at: 1,
            remote_uid: None,
        };

        let raw = String::from_utf8(build_draft_message(&draft).unwrap().formatted()).unwrap();
        assert!(raw.contains("Bcc: blind@example.com"));
    }

    #[test]
    fn smtp_delivery_sends_the_frozen_message_to_the_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            writer.write_all(b"220 test smtp\r\n").unwrap();
            let mut reader = BufReader::new(stream);
            let mut data = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if line.starts_with("EHLO") {
                    writer
                        .write_all(b"250-test\r\n250 AUTH PLAIN LOGIN\r\n")
                        .unwrap();
                } else if line.starts_with("AUTH") {
                    writer.write_all(b"235 authenticated\r\n").unwrap();
                } else if line.starts_with("MAIL FROM") || line.starts_with("RCPT TO") {
                    writer.write_all(b"250 ok\r\n").unwrap();
                } else if line.starts_with("DATA") {
                    writer.write_all(b"354 send it\r\n").unwrap();
                    loop {
                        let mut body_line = String::new();
                        reader.read_line(&mut body_line).unwrap();
                        if body_line == ".\r\n" {
                            break;
                        }
                        data.push_str(&body_line);
                    }
                    writer.write_all(b"250 queued\r\n").unwrap();
                    tx.send(std::mem::take(&mut data)).unwrap();
                } else if line.starts_with("QUIT") {
                    writer.write_all(b"221 bye\r\n").unwrap();
                    break;
                } else {
                    writer.write_all(b"250 ok\r\n").unwrap();
                }
            }
        });
        let form = MailboxForm {
            email: "john@example.com".into(),
            imap_host: "127.0.0.1".into(),
            imap_port: 0,
            imap_security: MailSecurity::None,
            smtp_host: "127.0.0.1".into(),
            smtp_port: port,
            smtp_security: MailSecurity::None,
        };
        send_outbox(&form, SmtpAuth::Password, "secret", &outgoing()).unwrap();
        let raw = rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert!(raw.contains("Subject: Hello"));
        assert!(raw.contains("Private body"));
        assert!(!raw.contains("secret"));
    }
}
