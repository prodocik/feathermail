//! T-082: proves `Core::dispatch(Command::Archive)` reaches a real `UID
//! MOVE` on a live (fake) IMAP server -- through the real `RemoteLocator`
//! (`Core` itself, `crates/core/src/locator.rs`, T-075), the real
//! `ImapMailProvider` (`crates/providers/src/apply.rs`, T-025), and the
//! real queue (`Core::tick_for_account`, T-078), not `FakeLocator` and not
//! a hand-assembled `Operation`.
//!
//! This could not be written inside `feathermail-core` (the fake IMAP
//! server lives in `feathermail-providers`'s tests) or inside
//! `feathermail-providers` (`Core.db` is `pub(crate)`, so seeding rows
//! from there would mean reaching into Core's schema from outside its own
//! crate, which is exactly what D9 forbids). `crates/service` is the
//! composition root that depends on both with no cycle, so it is the one
//! place a test can hold a real `Core` and a real fake-server-backed
//! `ImapMailProvider` at the same time.
//!
//! What this test seeds through raw SQL, and why: one `threads` row and
//! one `messages` row, standing in for "a message already synced from the
//! server". There is no public `Core` API for that -- writing synced
//! message metadata is `crates/sync`'s job (T-022), which would need this
//! fake server to also speak `UID FETCH`/`UIDVALIDITY`/`UIDNEXT`, well
//! beyond what T-082 is about. This mirrors the identical seed
//! `crates/core/src/locator.rs`'s own tests and
//! `crates/service/src/worker.rs`'s own tests already use, for the same
//! reason. Account and folder setup, by contrast, goes entirely through
//! the public surface: `Core::add_account`, a real `LIST` walk over the
//! fake server, and `Core::sync_folders`.

use std::path::Path;

use feathermail_core::{
    Command, ConnectError, ConnectOk, Core, MailConnector, MailSecurity, MailboxForm, ThreadId,
    TickOutcome, UndoReceipt,
};
use feathermail_providers::test_support::spawn_fake_imap_server;
use feathermail_providers::{discovered_folders, ImapAuth, ImapMailProvider, ImapSession};
use feathermail_service::ConnectorKind;
use feathermail_sync::{HeaderMeta, SyncStore};

const PASSWORD: &str = "fake-secret";

/// `Core::add_account` requires a working [`MailConnector::probe`].
/// Probing a real connector (SMTP included) is `crates/providers`'s own
/// job and already has its own tests; this fake server only speaks IMAP,
/// so probing it for real is not what this test is about. Same bypass
/// `crates/providers/src/apply.rs`'s own `NoProbeConnector` already uses.
struct NoProbeConnector;

impl MailConnector for NoProbeConnector {
    fn probe(&self, _form: &MailboxForm, _password: &str) -> Result<ConnectOk, ConnectError> {
        Ok(ConnectOk {
            capabilities: Vec::new(),
        })
    }
}

fn form(port: u16) -> MailboxForm {
    MailboxForm {
        email: "reader@example.com".into(),
        imap_host: "127.0.0.1".into(),
        imap_port: port,
        imap_security: MailSecurity::None,
        smtp_host: "127.0.0.1".into(),
        smtp_port: port,
        smtp_security: MailSecurity::None,
    }
}

/// No public `Core` API creates a message row that is already "synced
/// from the server" (see this file's top comment) -- so this reaches
/// straight into SQLite with a second `rusqlite` connection onto the same
/// on-disk WAL file the test's own `Core::open` handle has open, exactly
/// as `crates/core/src/locator.rs`'s and `crates/service/src/worker.rs`'s
/// own tests already do.
fn seed_synced_thread(
    db_path: &Path,
    account_id: &str,
    folder_id: &str,
    thread_id: &str,
    provider_uid: u32,
) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
         VALUES (?1, ?2, ?3, 'Hello', 'Hi', 0, 0)",
        rusqlite::params![thread_id, account_id, folder_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO messages (
             id, account_id, thread_id, folder_id, date, provider_uid,
             message_id_header, sender_name, sender_email, subject, size_bytes
         ) VALUES (?1, ?2, ?3, ?4, 1704103200, ?5,
                   '<m1@x>', 'Sender', 'sender@example.com', 'Hello', 100)",
        rusqlite::params![
            format!("{thread_id}-m1"),
            account_id,
            thread_id,
            folder_id,
            provider_uid
        ],
    )
    .unwrap();
}

fn operation_status_by_id(db_path: &Path, operation_id: &str) -> String {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT status FROM operations WHERE id = ?1",
        rusqlite::params![operation_id],
        |row| row.get(0),
    )
    .unwrap()
}

fn moved_header(uid: u32) -> HeaderMeta {
    HeaderMeta {
        uid,
        flags: vec!["\\Seen".into()],
        internaldate: None,
        size_bytes: Some(100),
        message_id: Some("<m1@x>".into()),
        in_reply_to: None,
        references: Vec::new(),
        from: Some("Sender <sender@example.com>".into()),
        to: Some("dest@example.com".into()),
        cc: None,
        subject: Some("Hello".into()),
        date: Some("01 Jan 2024 10:00:00 +0000".into()),
        gm_thrid: None,
    }
}

fn folder_id_for_kind(db_path: &Path, account_id: &str, kind: &str) -> String {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT id FROM folders WHERE account_id = ?1 AND kind = ?2",
        rusqlite::params![account_id, kind],
        |row| row.get(0),
    )
    .unwrap()
}

fn operation_status(db_path: &Path, target_id: &str) -> String {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT status FROM operations WHERE target_id = ?1",
        rusqlite::params![target_id],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn archive_command_reaches_a_real_uid_move_through_the_real_locator_and_queue() {
    // The destination mailbox is deliberately named nothing like
    // "archive": if resolution ever regressed to a guessed/hardcoded name
    // instead of a real `LIST SPECIAL-USE` walk, this mailbox would never
    // be found and the test would fail loudly instead of passing by
    // coincidence.
    const DEST: &str = "Vault/Sealed";
    let (port, state) = spawn_fake_imap_server(
        vec![("INBOX", vec![101]), (DEST, Vec::new())],
        true, // MOVE capability present -- exercise real UID MOVE, not the fallback.
    );
    state.lock().unwrap().tag_special_use(DEST, "\\Archive");

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("mail.db");

    let mut core = Core::open(&db_path).unwrap();
    let account_id = core
        .add_account(&form(port), PASSWORD, &NoProbeConnector)
        .unwrap();

    // Real LIST walk against the fake server (T-077's path), so the
    // Archive-kind destination is discovered under its own server name --
    // not assumed to be called "Archive".
    let mut discovery_session = ImapSession::connect(&form(port), ImapAuth::Login(PASSWORD.into()))
        .expect("connect for the LIST walk");
    let listings = discovery_session
        .list_folders()
        .expect("LIST SPECIAL-USE walk");
    // Close this connection before opening the next one: the fake server
    // serves one connection at a time (see `test_support`'s doc comment)
    // and only returns to `accept()` once this socket's other end goes
    // away, so leaving it open would hang the second `ImapSession::connect`
    // below waiting for a greeting nobody sends yet.
    drop(discovery_session);
    let discovered = discovered_folders(&listings);
    core.sync_folders(&account_id, &discovered).unwrap();

    let inbox_id = folder_id_for_kind(&db_path, account_id.as_str(), "inbox");
    let thread_id = "t1";
    seed_synced_thread(&db_path, account_id.as_str(), &inbox_id, thread_id, 101);

    // Offline: `dispatch` only ever touches SQLite -- no server connection
    // exists yet at this point.
    core.dispatch(Command::Archive {
        account_id: account_id.clone(),
        thread_ids: vec![ThreadId(thread_id.into())],
    })
    .unwrap();
    assert_eq!(
        operation_status(&db_path, thread_id),
        "pending",
        "must be queued locally, not yet applied"
    );

    // The connection "appears" only now, mirroring
    // `crates/service/src/provider_factory.rs`'s production wiring: a
    // second `Core::open` handle on the same on-disk file backs the
    // `RemoteLocator` (WAL mode supports two handles on one file, D13),
    // and login goes straight to the fake server with a known password
    // instead of through the keyring -- the same substitution
    // `ImapProviderFactory::connect` makes for `LibsecretStore`.
    let saved = core.account_connection(&account_id).unwrap();
    let auth = ConnectorKind::from_provider_str(&saved.provider).imap_auth(PASSWORD.into());
    let apply_session = ImapSession::connect(&saved.form, auth).expect("connect for apply");
    let locator = Core::open(&db_path).unwrap();
    let mut provider = ImapMailProvider::new(apply_session, locator);

    let outcome = core.tick_for_account(&account_id, &mut provider).unwrap();
    assert!(
        matches!(outcome, TickOutcome::Acked(_)),
        "expected the queued Archive to be acked, got {outcome:?}"
    );
    assert_eq!(operation_status(&db_path, thread_id), "acked");

    let st = state.lock().unwrap();
    assert!(
        st.mailboxes["INBOX"].is_empty(),
        "message must have left INBOX on the real fake server"
    );
    assert_eq!(
        st.mailboxes[DEST].len(),
        1,
        "server must see the archived message land in the mailbox a real \
         LIST SPECIAL-USE walk actually reported -- not a hardcoded 'Archive'"
    );
    // End-state alone (message left INBOX, message is in DEST) cannot
    // distinguish a real `UID MOVE` from the `UID COPY` + `UID STORE
    // \Deleted` + `EXPUNGE` fallback -- both leave the same mailboxes
    // behind. The fake server advertised MOVE in its CAPABILITY response,
    // so the real wire command must have been `UID MOVE`.
    assert_eq!(
        st.uid_move_calls, 1,
        "must have gone over the wire as UID MOVE"
    );
    assert_eq!(
        st.uid_copy_calls, 0,
        "must not have taken the no-MOVE-capability fallback path"
    );
}

#[test]
fn archive_ack_sync_then_undo_moves_back_with_the_new_destination_uid() {
    const DEST: &str = "Vault/Sealed";
    let (port, state) =
        spawn_fake_imap_server(vec![("INBOX", vec![101]), (DEST, Vec::new())], true);
    state.lock().unwrap().tag_special_use(DEST, "\\Archive");

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("mail.db");
    let mut core = Core::open(&db_path).unwrap();
    let account_id = core
        .add_account(&form(port), PASSWORD, &NoProbeConnector)
        .unwrap();

    let mut discovery_session =
        ImapSession::connect(&form(port), ImapAuth::Login(PASSWORD.into())).unwrap();
    let listings = discovery_session.list_folders().unwrap();
    drop(discovery_session);
    core.sync_folders(&account_id, &discovered_folders(&listings))
        .unwrap();

    let inbox_id = folder_id_for_kind(&db_path, account_id.as_str(), "inbox");
    seed_synced_thread(&db_path, account_id.as_str(), &inbox_id, "t-undo", 101);
    let receipt = core
        .dispatch_with_receipt(Command::Archive {
            account_id: account_id.clone(),
            thread_ids: vec![ThreadId("t-undo".into())],
        })
        .unwrap();
    let operation = receipt.operations.first().unwrap().operation_id.clone();

    let saved = core.account_connection(&account_id).unwrap();
    let auth = ConnectorKind::from_provider_str(&saved.provider).imap_auth(PASSWORD.into());
    let apply_session = ImapSession::connect(&saved.form, auth).unwrap();
    let locator = Core::open(&db_path).unwrap();
    let mut provider = ImapMailProvider::new(apply_session, locator);
    assert!(matches!(
        core.tick_for_account(&account_id, &mut provider).unwrap(),
        TickOutcome::Acked(_)
    ));

    {
        let st = state.lock().unwrap();
        assert!(st.mailboxes["INBOX"].iter().all(|m| m.uid != 101));
        assert_eq!(st.mailboxes[DEST].len(), 1);
        assert_eq!(st.mailboxes[DEST][0].uid, 102);
        assert_eq!(st.uid_move_calls, 1);
    }
    assert_eq!(operation_status(&db_path, "t-undo"), "acked");

    // This is the real source-vanish half of reconciliation. The fake server
    // has already assigned UID 102 in the destination; the local row must
    // survive the old UID's disappearance without inventing a new message.
    core.sync_store(&account_id, &inbox_id)
        .remove_vanished("INBOX", &[101])
        .unwrap();
    let archive_id = folder_id_for_kind(&db_path, account_id.as_str(), "archive");
    core.sync_store(&account_id, &archive_id)
        .upsert_headers(DEST, &[moved_header(102)])
        .unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let row: (i64, String, i64) = conn
        .query_row(
            "SELECT COUNT(*), folder_id, provider_uid FROM messages WHERE thread_id = 't-undo'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, (1, archive_id, 102));
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM operation_move_history WHERE operation_id = ?1",
            rusqlite::params![operation.as_str()],
            |r| r.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );

    // Undo after ACK must use the reconciled destination UID (102), never
    // replay the original source UID (101), and must create a durable reverse
    // operation rather than mutating the old ACKed row.
    let reverse = match core.undo(&receipt.operations[0].undo_ticket()).unwrap() {
        UndoReceipt::ReverseQueued {
            reverse_operation_id,
            ..
        } => reverse_operation_id,
        other => panic!("expected a post-ACK reverse operation, got {other:?}"),
    };
    assert_eq!(
        operation_status_by_id(&db_path, reverse.as_str()),
        "pending"
    );
    assert!(matches!(
        core.tick_for_account(&account_id, &mut provider).unwrap(),
        TickOutcome::Acked(_)
    ));

    let st = state.lock().unwrap();
    assert_eq!(st.uid_move_calls, 2);
    assert!(
        st.mailboxes["INBOX"].iter().any(|m| m.uid == 103),
        "reverse MOVE must create a fresh destination UID in INBOX"
    );
    assert!(
        st.mailboxes["INBOX"].iter().all(|m| m.uid != 101),
        "reverse MOVE must not reuse the original source UID"
    );
    assert!(st.mailboxes[DEST].is_empty());
}
