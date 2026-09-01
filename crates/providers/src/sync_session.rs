//! `impl MailboxSession for ImapSession` -- T-022 second half, the live-IMAP
//! side of the sync engine's world.
//!
//! This lives here, in `feathermail-providers`, rather than in
//! `feathermail-sync` itself, on purpose: `feathermail_sync::MailboxSession`
//! is a foreign trait from this crate's point of view, and `ImapSession`
//! (`crate::session::ImapSession`) is foreign from `feathermail-sync`'s
//! point of view, so the *only* crate where this `impl` satisfies Rust's
//! orphan rule is one that owns one side of it -- here, the type. Keeping
//! `feathermail-sync` itself free of any concrete dependency (no `db`, no
//! `providers`, no `rusqlite`) is deliberate (D9): it stays pure trait +
//! pure logic, and every crate that wants to *drive* the engine with a real
//! implementation supplies its own adapter over its own types. The SQLite
//! side of this same split ([`feathermail_sync::SyncStore`]) lives in
//! `feathermail-core`, which owns `Database`, for the identical reason.
//!
//! `ImapSession` already exposes almost exactly the shape
//! [`MailboxSession`] needs (`select`, `uid_fetch_headers`,
//! `uid_fetch_flags_changed_since`, `list_folders`); this impl is mostly
//! field-for-field remapping between this crate's wire types
//! ([`SelectedMailbox`], `crate::HeaderMeta`, `crate::UidRange`) and the
//! engine's equivalents, plus folding [`ConnectError`] into
//! [`SyncError::Session`]. IMAP itself requires a mailbox to be `SELECT`ed
//! before any `UID FETCH` against it; the sync engine already guarantees
//! that ordering (`sync_folder` always calls `select` before issuing any
//! fetch for the same folder in the same pass), so this impl does not
//! track "currently selected folder" state of its own -- it trusts the
//! engine's call order rather than re-deriving it.

use feathermail_core::ConnectError;
use feathermail_sync::{
    HeaderMeta as SyncHeaderMeta, MailboxSession, MailboxSnapshot, SyncError,
    UidRange as SyncUidRange,
};

use crate::{FolderListing, HeaderMeta, ImapSession, SelectedMailbox, UidRange};

impl MailboxSession for ImapSession {
    fn select(&mut self, folder: &str) -> Result<MailboxSnapshot, SyncError> {
        let selected: SelectedMailbox = ImapSession::select(self, folder).map_err(map_err)?;
        Ok(MailboxSnapshot {
            uidvalidity: selected.uidvalidity,
            uidnext: selected.uidnext,
            exists: selected.exists,
            highest_modseq: selected.highest_modseq,
        })
    }

    fn uid_fetch_headers(
        &mut self,
        _folder: &str,
        range: SyncUidRange,
    ) -> Result<Vec<SyncHeaderMeta>, SyncError> {
        let headers =
            ImapSession::uid_fetch_headers(self, to_wire_range(range)).map_err(map_err)?;
        Ok(headers.into_iter().map(from_wire_header).collect())
    }

    fn uid_fetch_flags_changed_since(
        &mut self,
        _folder: &str,
        range: SyncUidRange,
        modseq: u64,
    ) -> Result<Vec<SyncHeaderMeta>, SyncError> {
        let headers =
            ImapSession::uid_fetch_flags_changed_since(self, to_wire_range(range), modseq)
                .map_err(map_err)?;
        Ok(headers.into_iter().map(from_wire_header).collect())
    }

    fn list_folders(&mut self) -> Result<Vec<String>, SyncError> {
        let folders: Vec<FolderListing> = ImapSession::list_folders(self).map_err(map_err)?;
        Ok(folders.into_iter().map(|f| f.name).collect())
    }

    /// T-024: the trait method this crate's `ImapSession::uid_fetch_body_peek`
    /// exists to back. `folder` is unused here for the same reason it's
    /// unused on `uid_fetch_headers` above -- this impl trusts the engine's
    /// call order (`select` before any fetch for that folder), it does not
    /// re-derive "currently selected folder" state of its own.
    /// `uid_fetch_body_peek` already uses `BODY.PEEK[]`, never bare
    /// `BODY[]`, so this never marks a message `\Seen` as a side effect.
    fn fetch_body(&mut self, _folder: &str, uid: u32) -> Result<Vec<u8>, SyncError> {
        ImapSession::uid_fetch_body_peek(self, uid).map_err(map_err)
    }

    /// The whole point of the batch: one `UID FETCH` for the set, not one
    /// per UID. Overrides the trait's looping default -- see
    /// [`feathermail_sync::MailboxSession::fetch_bodies`].
    fn fetch_bodies(
        &mut self,
        _folder: &str,
        uids: &[u32],
    ) -> Result<Vec<(u32, Vec<u8>)>, SyncError> {
        ImapSession::uid_fetch_bodies_peek(self, uids).map_err(map_err)
    }
}

fn to_wire_range(r: SyncUidRange) -> UidRange {
    match r.to {
        Some(to) => UidRange::bounded(r.from, to),
        None => UidRange::open(r.from),
    }
}

fn from_wire_header(h: HeaderMeta) -> SyncHeaderMeta {
    SyncHeaderMeta {
        uid: h.uid,
        flags: h.flags,
        internaldate: h.internaldate,
        size_bytes: h.size_bytes,
        message_id: h.message_id,
        in_reply_to: h.in_reply_to,
        references: h.references,
        from: h.from,
        to: h.to,
        cc: h.cc,
        subject: h.subject,
        date: h.date,
        gm_thrid: h.gm_thrid,
    }
}

/// `ConnectError::Auth` folds to [`SyncError::Auth`] (T-091), dropping its
/// `message`/`details` entirely -- symmetric to `ApplyError::Auth`'s own
/// bare-tag shape, and load-bearing for D14: whatever text produced the
/// `Auth` classification must not leave this function. The other two
/// variants carry `message`/`details`; neither ever holds a password,
/// token, or body (see `wire::sanitize`, which every call site already
/// routes raw server text through), so folding both into one string for
/// [`SyncError::Session`] does not risk leaking a secret.
///
/// # Why this rarely produces `SyncError::Auth` today
///
/// `ConnectError::Auth` is only ever returned by `wire::imap_login` /
/// `wire::imap_authenticate_xoauth2` -- both reached exclusively from
/// `ImapSession::connect`, never from `select`/`run_fetch`/`list_folders`/
/// `uid_fetch_body_peek` (this impl's actual callers, see the trait impl
/// above), which always fold a non-`OK` tagged response into
/// `ConnectError::network(...)` regardless of what the response code says.
/// There is currently no protocol-level place in this crate that tells "the
/// access token died mid-session" apart from any other `SELECT`/`FETCH`
/// failure -- IMAP servers are free to send a response code like
/// `[AUTHENTICATIONFAILED]` on a post-LOGIN command, but nothing here
/// parses IMAP response codes off an arbitrary tagged line (only
/// `UIDVALIDITY`/`UIDNEXT`/`HIGHESTMODSEQ` are extracted, and only from a
/// successful `SELECT`). Guessing from the response *text* instead (a
/// substring match for "authenticationfailed" or similar) is exactly the
/// server-message-dependent heuristic T-091 was told not to invent. So
/// this match arm exists for correctness and forward-compatibility -- the
/// day a genuine protocol-level signal for "auth died mid-session" is
/// added to `ImapSession`, it flows through here unchanged -- not because
/// it is reachable from a live IMAP server today. See
/// `crates/service/src/provider_factory.rs`'s own doc comment on
/// `ReauthingProvider`'s `MailSession` impl for the same fact stated from
/// the worker's side.
fn map_err(e: ConnectError) -> SyncError {
    match e {
        ConnectError::Auth { .. } => SyncError::Auth,
        ConnectError::Network { message, details } | ConnectError::Invalid { message, details } => {
            SyncError::Session(match details {
                Some(d) => format!("{message}: {d}"),
                None => message,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_core::{MailSecurity, MailboxForm};
    use feathermail_sync::UidRange as EngineUidRange;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    use crate::ImapAuth;

    fn form(port: u16) -> MailboxForm {
        MailboxForm {
            email: "you@example.com".into(),
            imap_host: "127.0.0.1".into(),
            imap_port: port,
            imap_security: MailSecurity::None,
            smtp_host: "127.0.0.1".into(),
            smtp_port: 0,
            smtp_security: MailSecurity::None,
        }
    }

    /// Minimal fake server exercising just enough of the protocol to check
    /// the [`MailboxSession`] impl's field mapping and error mapping --
    /// `crate::session`'s own tests already cover the wire parsing itself in
    /// depth, so this does not repeat that.
    fn spawn_fake_server(select_bad: bool) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let upper = line.to_ascii_uppercase();
                let tag = line.split_whitespace().next().unwrap_or("*").to_string();
                if upper.contains(" LOGIN ") {
                    write!(writer, "{tag} OK logged in\r\n").unwrap();
                } else if upper.contains(" CAPABILITY") {
                    write!(
                        writer,
                        "* CAPABILITY IMAP4rev1\r\n{tag} OK CAPABILITY completed\r\n"
                    )
                    .unwrap();
                } else if upper.contains(" LIST ") {
                    write!(writer, "* LIST (\\HasNoChildren) \"/\" INBOX\r\n").unwrap();
                    write!(
                        writer,
                        "* LIST (\\HasNoChildren \\Sent) \"/\" \"Sent Items\"\r\n"
                    )
                    .unwrap();
                    write!(writer, "{tag} OK LIST completed\r\n").unwrap();
                } else if upper.contains(" SELECT ") {
                    if select_bad {
                        write!(writer, "{tag} BAD no such mailbox\r\n").unwrap();
                    } else {
                        write!(writer, "* FLAGS (\\Answered \\Seen)\r\n").unwrap();
                        write!(writer, "* 3 EXISTS\r\n").unwrap();
                        write!(writer, "* OK [UIDVALIDITY 123456] UIDs valid\r\n").unwrap();
                        write!(writer, "* OK [UIDNEXT 42] Predicted next UID\r\n").unwrap();
                        write!(writer, "* OK [HIGHESTMODSEQ 999] highest\r\n").unwrap();
                        write!(writer, "{tag} OK [READ-WRITE] SELECT completed\r\n").unwrap();
                    }
                } else if upper.contains("UID FETCH") {
                    let header = "Subject: Hi there\r\nFrom: a@b.com\r\nMessage-ID: <abc@x>\r\n";
                    write!(
                        writer,
                        "* 1 FETCH (UID 7 FLAGS (\\Seen) INTERNALDATE \"01-Jan-2024 00:00:00 +0000\" RFC822.SIZE 1234 BODY[HEADER.FIELDS (MESSAGE-ID IN-REPLY-TO REFERENCES FROM TO SUBJECT DATE)] {{{}}}\r\n",
                        header.len()
                    )
                    .unwrap();
                    write!(writer, "{header}").unwrap();
                    write!(writer, ")\r\n").unwrap();
                    write!(writer, "{tag} OK UID FETCH completed\r\n").unwrap();
                } else if upper.contains("LOGOUT") {
                    write!(writer, "* BYE\r\n{tag} OK LOGOUT\r\n").unwrap();
                    break;
                } else {
                    write!(writer, "{tag} BAD unknown\r\n").unwrap();
                }
                writer.flush().unwrap();
            }
        });
        port
    }

    #[test]
    fn select_maps_to_engine_mailbox_snapshot() {
        let port = spawn_fake_server(false);
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let snapshot = MailboxSession::select(&mut session, "INBOX").unwrap();
        assert_eq!(snapshot.uidvalidity, 123456);
        assert_eq!(snapshot.uidnext, 42);
        assert_eq!(snapshot.exists, 3);
        assert_eq!(snapshot.highest_modseq, Some(999));
    }

    #[test]
    fn uid_fetch_headers_maps_to_engine_header_meta() {
        let port = spawn_fake_server(false);
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();
        MailboxSession::select(&mut session, "INBOX").unwrap();

        let headers = MailboxSession::uid_fetch_headers(
            &mut session,
            "INBOX",
            EngineUidRange::bounded(1, 100),
        )
        .unwrap();
        assert_eq!(headers.len(), 1);
        let h = &headers[0];
        assert_eq!(h.uid, 7);
        assert_eq!(h.flags, vec!["\\Seen".to_string()]);
        assert_eq!(h.subject.as_deref(), Some("Hi there"));
        assert_eq!(h.from.as_deref(), Some("a@b.com"));
        assert_eq!(h.message_id.as_deref(), Some("<abc@x>"));
        assert_eq!(h.size_bytes, Some(1234));
    }

    #[test]
    fn list_folders_returns_names_only() {
        let port = spawn_fake_server(false);
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let names = MailboxSession::list_folders(&mut session).unwrap();
        assert!(names.contains(&"INBOX".to_string()));
        assert!(names.contains(&"Sent Items".to_string()));
    }

    #[test]
    fn protocol_error_maps_to_sync_session_error() {
        let port = spawn_fake_server(true);
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let err = MailboxSession::select(&mut session, "INBOX").unwrap_err();
        match err {
            SyncError::Session(msg) => assert!(!msg.is_empty()),
            other => panic!("expected SyncError::Session, got {other:?}"),
        }
    }

    /// `MailboxSession::fetch_body` reuses `spawn_fake_server`'s existing
    /// `UID FETCH` handler -- this only checks that the trait method
    /// forwards bytes / folds `ConnectError` into `SyncError` correctly,
    /// not wire-level parsing details (that's `session`'s own test
    /// module).
    #[test]
    fn fetch_body_forwards_bytes_on_success() {
        let port = spawn_fake_server(false);
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let bytes = MailboxSession::fetch_body(&mut session, "INBOX", 7).unwrap();
        assert!(!bytes.is_empty());
    }

    fn spawn_fetch_rejecting_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            write!(writer, "* OK fake ready\r\n").unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let tag = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "{tag} OK logged in\r\n").unwrap();
            writer.flush().unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let tag2 = line.split_whitespace().next().unwrap().to_string();
            write!(writer, "{tag2} NO no such message\r\n").unwrap();
            writer.flush().unwrap();
        });
        port
    }

    #[test]
    fn fetch_body_maps_connect_error_to_sync_error() {
        let port = spawn_fetch_rejecting_server();
        thread::sleep(Duration::from_millis(30));
        let mut session = ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap();

        let err = MailboxSession::fetch_body(&mut session, "INBOX", 7).unwrap_err();
        match err {
            SyncError::Session(msg) => assert!(!msg.is_empty()),
            other => panic!("expected SyncError::Session, got {other:?}"),
        }
    }

    // --- `map_err` (T-091): `ConnectError::Auth` folds to `SyncError::Auth`,
    // never `SyncError::Session`, and drops the message/details entirely --
    // see `map_err`'s own doc comment for why this branch cannot currently
    // be reached from a live `ImapSession` call (there is no test against a
    // fake server for it here for exactly that reason: nothing in
    // `crate::session` ever constructs `ConnectError::Auth` outside of
    // `connect`, which `MailboxSession`'s impl never calls). These tests
    // exercise the pure function directly instead.

    #[test]
    fn map_err_folds_connect_auth_into_sync_auth_not_session() {
        let err = map_err(ConnectError::auth("token rejected"));
        assert_eq!(err, SyncError::Auth);
    }

    #[test]
    fn map_err_still_folds_network_and_invalid_into_sync_session() {
        assert!(matches!(
            map_err(ConnectError::network("connection reset")),
            SyncError::Session(_)
        ));
        assert!(matches!(
            map_err(ConnectError::invalid("bad mailbox name")),
            SyncError::Session(_)
        ));
    }

    /// D14: whatever text a server sent back that made `map_err` classify
    /// this as an auth failure must not survive into `SyncError::Auth`'s
    /// `Debug` output -- it is a bare tag, not a wrapper around the
    /// message. Uses an obviously-fake, distinctive marker string so a
    /// future change that starts threading `message`/`details` through
    /// this branch would make this test fail loudly rather than by luck.
    #[test]
    fn map_err_does_not_leak_the_connect_error_text_into_sync_auth() {
        let secret_marker = "super-secret-token-xyz-should-never-appear";
        let err = map_err(ConnectError::auth(secret_marker));
        let debug = format!("{err:?}");
        assert!(
            !debug.contains(secret_marker),
            "SyncError::Auth's Debug output leaked the ConnectError's \
             message/details text: {debug:?}"
        );
        assert_eq!(err, SyncError::Auth);
    }
}
