//! Shared IMAP/SMTP socket helpers (T-018/T-019).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use feathermail_core::{ConnectError, MailSecurity, MailboxForm};
use native_tls::TlsConnector;

pub(crate) const TIMEOUT: Duration = Duration::from_secs(8);

pub(crate) fn lookup(host: &str, port: u16) -> Result<std::net::SocketAddr, ConnectError> {
    (host, port)
        .to_socket_addrs()
        .map_err(net)?
        .next()
        .ok_or_else(|| ConnectError::network(format!("no address for {host}")))
}

pub(crate) fn tls() -> Result<TlsConnector, ConnectError> {
    TlsConnector::builder().build().map_err(tls_err)
}

pub(crate) struct ImapConn {
    pub stream: ImapStream,
    pub read_greeting: bool,
}

pub(crate) fn imap_connect(form: &MailboxForm) -> Result<ImapConn, ConnectError> {
    let here = |e: std::io::Error| unreachable(MailLeg::Imap, &form.imap_host, form.imap_port, e);
    let addr = lookup(&form.imap_host, form.imap_port).map_err(|_| {
        unreachable(
            MailLeg::Imap,
            &form.imap_host,
            form.imap_port,
            "address lookup failed",
        )
    })?;
    let tcp = TcpStream::connect_timeout(&addr, TIMEOUT).map_err(here)?;
    tcp.set_read_timeout(Some(TIMEOUT)).map_err(net)?;
    tcp.set_write_timeout(Some(TIMEOUT)).map_err(net)?;
    match form.imap_security {
        MailSecurity::Ssl => {
            let tls = tls()?.connect(&form.imap_host, tcp).map_err(tls_err)?;
            Ok(ImapConn {
                stream: ImapStream::Tls(tls),
                read_greeting: true,
            })
        }
        MailSecurity::StartTls => {
            let mut stream = tcp;
            expect_greeting(&mut stream)?;
            tagged(&mut stream, "A1", "STARTTLS")?;
            let tls = tls()?.connect(&form.imap_host, stream).map_err(tls_err)?;
            Ok(ImapConn {
                stream: ImapStream::Tls(tls),
                read_greeting: false,
            })
        }
        MailSecurity::None => {
            let mut stream = tcp;
            expect_greeting(&mut stream)?;
            Ok(ImapConn {
                stream: ImapStream::Plain(stream),
                read_greeting: false,
            })
        }
    }
}

pub(crate) enum ImapStream {
    Plain(TcpStream),
    Tls(native_tls::TlsStream<TcpStream>),
}

impl Read for ImapStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for ImapStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

impl ImapStream {
    /// Bound how long the next read may block (T-026: IDLE has to poll for
    /// "did anything arrive yet?" without blocking forever, and without
    /// tearing the connection down just because nothing happened for a
    /// while). `None` restores blocking reads at the OS level; callers that
    /// want the normal command/response behavior back should pass
    /// `Some(TIMEOUT)` explicitly rather than `None`, since a genuinely
    /// unbounded read is not otherwise used anywhere in this module.
    pub(crate) fn set_read_timeout(&self, dur: Option<Duration>) -> Result<(), ConnectError> {
        match self {
            Self::Plain(s) => s.set_read_timeout(dur).map_err(net),
            Self::Tls(s) => s.get_ref().set_read_timeout(dur).map_err(net),
        }
    }
}

pub(crate) fn expect_greeting<S: Read>(stream: &mut S) -> Result<(), ConnectError> {
    let line = read_line(stream)?;
    if line.starts_with("* OK") || line.starts_with("* PREAUTH") {
        Ok(())
    } else {
        Err(ConnectError::network(sanitize(&line)))
    }
}

pub(crate) fn capability<S: Read + Write>(
    stream: &mut S,
    tag: &str,
) -> Result<Vec<String>, ConnectError> {
    write_cmd(stream, tag, "CAPABILITY")?;
    let lines = read_tagged(stream, tag)?;
    let mut caps = Vec::new();
    for line in &lines {
        if let Some(rest) = line.strip_prefix("* CAPABILITY ") {
            caps.extend(rest.split_whitespace().map(str::to_string));
        }
    }
    let last = lines.last().cloned().unwrap_or_default();
    if last.starts_with(&format!("{tag} OK")) {
        Ok(caps)
    } else {
        Err(ConnectError::network(sanitize(&last)))
    }
}

pub(crate) fn tagged<S: Read + Write>(
    stream: &mut S,
    tag: &str,
    command: &str,
) -> Result<(), ConnectError> {
    write_cmd(stream, tag, command)?;
    let lines = read_tagged(stream, tag)?;
    let last = lines.last().cloned().unwrap_or_default();
    if last.starts_with(&format!("{tag} OK")) {
        Ok(())
    } else {
        Err(ConnectError::network(sanitize(&last)))
    }
}

pub(crate) fn write_cmd<S: Write>(
    stream: &mut S,
    tag: &str,
    command: &str,
) -> Result<(), ConnectError> {
    write!(stream, "{tag} {command}\r\n").map_err(net)?;
    stream.flush().map_err(net)
}

pub(crate) fn read_tagged<S: Read>(stream: &mut S, tag: &str) -> Result<Vec<String>, ConnectError> {
    let mut lines = Vec::new();
    let prefix = format!("{tag} ");
    loop {
        let line = read_line(stream)?;
        let done = line.starts_with(&prefix);
        lines.push(line);
        if done {
            return Ok(lines);
        }
        if lines.len() > 64 {
            return Err(ConnectError::network("IMAP response too long"));
        }
    }
}

pub(crate) fn read_line<S: Read>(stream: &mut S) -> Result<String, ConnectError> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).map_err(net)?;
        if byte[0] == b'\n' {
            break;
        }
        if byte[0] != b'\r' {
            buf.push(byte[0]);
        }
        if buf.len() > 16_384 {
            return Err(ConnectError::network("IMAP line too long"));
        }
    }
    String::from_utf8(buf).map_err(|_| ConnectError::network("IMAP line is not utf-8"))
}

pub(crate) fn looks_like_auth(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    if t.contains("no compatible authentication mechanism") {
        return false;
    }
    t.contains("auth") || t.contains("login") || t.contains("credential") || t.contains("535")
}

pub(crate) fn sanitize(text: &str) -> String {
    text.replace(['\r', '\n'], " ").trim().to_string()
}

pub(crate) fn net(err: impl std::fmt::Display) -> ConnectError {
    ConnectError::network(err.to_string())
}

/// T-103: name the leg and the endpoint that did not answer.
///
/// The owner typed a Gmail address into Add account, waited on the spinner
/// and read "Couldn't reach the server." Both probes hide behind that one
/// sentence, so it does not say whether the mailbox is unreachable, the
/// outgoing server is, or the address is simply wrong -- and here it was
/// neither of the first two in general: IMAP 993 answered and SMTP 465 timed
/// out, because this network has no route to Google's submission port while
/// 587 works. The host and port are the values the owner just typed into the
/// form on their own screen; no credential is in this string (D14 -- the
/// protocol text stays in `details`, which never crosses the UI boundary).
pub(crate) fn unreachable(
    leg: MailLeg,
    host: &str,
    port: u16,
    details: impl std::fmt::Display,
) -> ConnectError {
    let hint = match leg {
        MailLeg::Imap => "",
        MailLeg::Smtp => " Some networks block this port; port 587 with STARTTLS usually works.",
    };
    ConnectError::Network {
        message: format!(
            "Couldn't reach the {} server {host}:{port}.{hint}",
            leg.name()
        ),
        details: Some(sanitize(&details.to_string())),
    }
}

/// Which half of a mailbox a probe was talking to when it failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MailLeg {
    Imap,
    Smtp,
}

impl MailLeg {
    fn name(self) -> &'static str {
        match self {
            Self::Imap => "incoming mail (IMAP)",
            Self::Smtp => "outgoing mail (SMTP)",
        }
    }
}

pub(crate) fn tls_err(err: impl std::fmt::Display) -> ConnectError {
    ConnectError::network(err.to_string())
}

pub(crate) fn smtp_err(err: impl std::fmt::Display) -> ConnectError {
    ConnectError::network(err.to_string())
}

// --- Shared auth wire helpers (T-022 session.rs reuses these instead of
// duplicating generic.rs::login / gmail.rs::authenticate_xoauth2, which stay
// private to their modules). Purely additive: nothing above this line moved. ---

pub(crate) fn imap_quote(s: &str) -> Result<String, ConnectError> {
    if s.bytes().any(|b| b == 0 || b == b'\r' || b == b'\n') {
        return Err(ConnectError::invalid("That mailbox value isn't valid."));
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// `tag LOGIN user pass`, password buffer zeroed after send (mirrors
/// `generic.rs::login`).
pub(crate) fn imap_login<S: Read + Write>(
    stream: &mut S,
    tag: &str,
    user: &str,
    password: &str,
) -> Result<(), ConnectError> {
    let quoted_user = imap_quote(user)?;
    let quoted_pass = imap_quote(password)?;
    let mut cmd = Vec::with_capacity(16 + quoted_user.len() + quoted_pass.len());
    cmd.extend_from_slice(tag.as_bytes());
    cmd.extend_from_slice(b" LOGIN ");
    cmd.extend_from_slice(quoted_user.as_bytes());
    cmd.push(b' ');
    cmd.extend_from_slice(quoted_pass.as_bytes());
    cmd.extend_from_slice(b"\r\n");
    stream.write_all(&cmd).map_err(net)?;
    stream.flush().map_err(net)?;
    cmd.fill(0);
    let lines = read_tagged(stream, tag)?;
    let last = lines.last().cloned().unwrap_or_default();
    if last.starts_with(&format!("{tag} OK")) {
        Ok(())
    } else {
        Err(ConnectError::auth(sanitize(&last)))
    }
}

/// `tag AUTHENTICATE XOAUTH2 <sasl>`, handling the RFC 7628 `+` error
/// continuation (mirrors `gmail.rs::authenticate_xoauth2`). Token is zeroed
/// after send.
pub(crate) fn imap_authenticate_xoauth2<S: Read + Write>(
    stream: &mut S,
    tag: &str,
    user: &str,
    access_token: &str,
) -> Result<(), ConnectError> {
    let mut encoded = crate::oauth::sasl_xoauth2(user, access_token);
    stream
        .write_all(format!("{tag} AUTHENTICATE XOAUTH2 ").as_bytes())
        .map_err(net)?;
    stream.write_all(&encoded).map_err(net)?;
    stream.write_all(b"\r\n").map_err(net)?;
    stream.flush().map_err(net)?;
    encoded.fill(0);
    let line = read_line(stream)?;
    if line.starts_with(&format!("{tag} OK")) {
        return Ok(());
    }
    if let Some(rest) = line.strip_prefix('+') {
        let _ = rest;
        stream.write_all(b"\r\n").map_err(net)?;
        stream.flush().map_err(net)?;
        let lines = read_tagged(stream, tag)?;
        let last = lines.last().cloned().unwrap_or_default();
        if last.starts_with(&format!("{tag} OK")) {
            return Ok(());
        }
        return Err(ConnectError::reauth(sanitize(&last)));
    }
    if line.starts_with(&format!("{tag} NO")) || line.starts_with(&format!("{tag} BAD")) {
        return Err(ConnectError::reauth(sanitize(&line)));
    }
    Err(ConnectError::network(sanitize(&line)))
}
