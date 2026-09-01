//! Applying queued operations to IMAP: flags, move, delete, folder create
//! (T-025, TZ §20/§69).
//!
//! [`ImapMailProvider`] implements [`MailProvider`] — the trait
//! `feathermail_core::queue::Core::tick` calls once a queued
//! [`Operation`] is claimed. It never touches SQLite itself (D9: schema
//! knowledge like `messages.provider_uid` / `folders.remote_id` stays out
//! of this crate); instead it asks a small [`RemoteLocator`] for the IMAP
//! coordinates (mailbox + UID) an operation's local `target_id`/`payload`
//! refer to. The composition root backs that trait against
//! `feathermail_core::Core`; this module's tests back it with a fake.
//!
//! Wire mapping (ТЗ §20/§69):
//! - `MarkRead`/`MarkUnread` → `UID STORE (+|-)FLAGS (\Seen)`
//! - `Star`/`Unstar` → `UID STORE (+|-)FLAGS (\Flagged)`
//! - `Trash`/`Archive`/`Move` → `UID MOVE` into the destination mailbox, or
//!   on a server without `MOVE` in `CAPABILITY`: `UID COPY` +
//!   `UID STORE +FLAGS (\Deleted)` + `EXPUNGE`. Locally `Trash` moves the
//!   thread out of its folder just like `Archive` does (`store.rs` sets
//!   `deleted = 1`, and `Core::list_folders` filters Trash by exactly that
//!   flag) — so on the wire it must leave the source mailbox too. A bare
//!   `\Deleted` flag with no `EXPUNGE`/`MOVE` would leave the message
//!   sitting in its original mailbox forever: sync's `remove_vanished`
//!   only reaps UIDs actually absent from a `FETCH` response, and a
//!   `\Deleted`-flagged-but-not-expunged message is still present in it.
//!   The destination mailbox is resolved through [`RemoteLocator`] exactly
//!   like `Archive`'s; if it can't be resolved (no `\Trash` special-use
//!   mailbox, none configured), the operation errors out and stays queued
//!   rather than guessing a mailbox name.
//! - `PermanentDelete` → `UID STORE +FLAGS (\Deleted)` + scoped `UID EXPUNGE`
//!   on UIDPLUS servers, without resolving or writing a Trash mailbox. A
//!   server without UIDPLUS is rejected rather than using broad `EXPUNGE`
//!   and risking an unrelated already-deleted message.
//! - `CreateFolder` → `CREATE`; a "mailbox already exists" response is
//!   treated as success (D29: idempotent)
//! - `RenameFolder` → `RENAME`, with both mailbox paths taken verbatim from
//!   the payload Core queued. Unlike `CREATE` there is no "already so"
//!   response to forgive: if the source is gone or the destination exists,
//!   the server's `NO` is the truth and the operation fails.
//! - `Snooze` has no IMAP representation (client-local only) and always
//!   acks; `Send` is SMTP/outbox territory, not this provider's job.

use feathermail_core::model::{AccountId, OpKind, Operation};
use feathermail_core::provider::{ApplyError, MailProvider};
use feathermail_core::ConnectError;

use crate::session::ImapSession;

// `RemoteLocator`, `RemoteMessage`, `ARCHIVE_FOLDER_KEY`, `TRASH_FOLDER_KEY`
// used to be declared in this module. T-075 moved them into
// `feathermail-core` (`crates/core/src/provider.rs`): the real
// implementation needs `Core`'s private SQLite handle, and `crates/core`
// cannot depend back on `crates/providers` (D9). Re-exported here so this
// crate's own callers see no difference.
pub use feathermail_core::provider::{
    RemoteLocator, RemoteMessage, ARCHIVE_FOLDER_KEY, TRASH_FOLDER_KEY,
};

/// The one mailbox every IMAP server is required to have (RFC 3501 5.1),
/// used by [`ImapMailProvider::delete_folder`] purely as somewhere safe to
/// stand while deleting a different mailbox.
const INBOX_MAILBOX: &str = "INBOX";

/// Applies queued [`Operation`]s over one live [`ImapSession`] (T-025).
pub struct ImapMailProvider<L: RemoteLocator> {
    session: ImapSession,
    locator: L,
    selected: Option<String>,
}

impl<L: RemoteLocator> ImapMailProvider<L> {
    pub fn new(session: ImapSession, locator: L) -> Self {
        Self {
            session,
            locator,
            selected: None,
        }
    }

    /// Exposes the live session underneath this provider so
    /// `feathermail_sync::sync_folder` (T-078 (b)) can drive it directly --
    /// `sync_folder` needs `&mut impl MailboxSession`, and `ImapSession`
    /// already implements that (`crate::sync_session`), so there is
    /// nothing left to adapt here beyond handing out the field.
    ///
    /// # Why this resets `selected` -- read before deleting this line
    ///
    /// `sync_folder` calls `session.select(folder)` on whatever session it
    /// is given, on its own schedule, with no way to tell *this* provider
    /// that it happened -- it only knows about `MailboxSession`, not about
    /// `ImapMailProvider` or its private `selected` cache. If this method
    /// only returned `&mut self.session` and left `selected` untouched, the
    /// next queued operation applied through this provider's
    /// [`MailProvider::apply`] would call [`Self::ensure_selected`], see
    /// its cached folder name still matching what it *thinks* is selected,
    /// skip re-issuing `SELECT` -- and send its `UID STORE`/`UID MOVE`
    /// against whatever mailbox `sync_folder` actually left selected on
    /// the wire instead, silently corrupting an unrelated mailbox. This is
    /// not hypothetical caution: this exact defect class -- this
    /// provider's idea of "what's selected" drifting from the wire truth
    /// because something bypassed `ensure_selected` -- was already caught
    /// once during T-078 (a), which is precisely why `ensure_selected`'s
    /// cache exists in the first place: to skip redundant `SELECT`s
    /// *without* ever being wrong about what's actually selected.
    /// Resetting `selected` to `None` here is not defensive over-caution;
    /// it is the one line that keeps that cache honest across a caller it
    /// cannot see into.
    pub fn mailbox(&mut self) -> &mut ImapSession {
        self.selected = None;
        &mut self.session
    }

    fn ensure_selected(&mut self, folder: &str) -> Result<(), ApplyError> {
        if self.selected.as_deref() == Some(folder) {
            return Ok(());
        }
        self.session.select(folder).map_err(connect_to_apply)?;
        self.selected = Some(folder.to_string());
        Ok(())
    }

    fn store_flag(
        &mut self,
        account_id: &AccountId,
        thread_id: &str,
        flag: &str,
        add: bool,
    ) -> Result<(), ApplyError> {
        let messages = self.locator.thread_messages(account_id, thread_id)?;
        if messages.is_empty() {
            return Err(ApplyError::NotFound);
        }
        for (folder, uids) in group_by_folder(&messages) {
            self.ensure_selected(&folder)?;
            self.session
                .uid_store_flag(&uids, flag, add)
                .map_err(connect_to_apply)?;
        }
        Ok(())
    }

    fn move_thread(
        &mut self,
        account_id: &AccountId,
        thread_id: &str,
        dest_key: &str,
        operation_id: &str,
    ) -> Result<(), ApplyError> {
        let messages =
            self.locator
                .thread_messages_for_operation(account_id, thread_id, operation_id)?;
        if messages.is_empty() {
            return Err(ApplyError::NotFound);
        }
        let dest = self
            .locator
            .remote_folder_for_operation(account_id, dest_key, operation_id)?;
        let can_move = self
            .session
            .capabilities()
            .map_err(connect_to_apply)?
            .can_move;
        for (folder, uids) in group_by_folder(&messages) {
            if folder == dest {
                // Already there (D29): nothing left to do for this group.
                continue;
            }
            self.ensure_selected(&folder)?;
            if can_move {
                self.session
                    .uid_move(&uids, &dest)
                    .map_err(connect_to_apply)?;
            } else {
                self.session
                    .uid_copy(&uids, &dest)
                    .map_err(connect_to_apply)?;
                self.session
                    .uid_store_flag(&uids, "\\Deleted", true)
                    .map_err(connect_to_apply)?;
                self.session.expunge().map_err(connect_to_apply)?;
            }
            // MOVE/EXPUNGE can renumber or change EXISTS in the mailbox we
            // had selected; never trust `selected` to still be accurate.
            self.selected = None;
        }
        Ok(())
    }

    fn permanently_delete_thread(
        &mut self,
        account_id: &AccountId,
        thread_id: &str,
    ) -> Result<(), ApplyError> {
        let messages = self.locator.thread_messages(account_id, thread_id)?;
        if messages.is_empty() {
            return Err(ApplyError::NotFound);
        }
        let uidplus = self
            .session
            .capabilities()
            .map_err(connect_to_apply)?
            .uidplus;
        if !uidplus {
            // Plain EXPUNGE is mailbox-wide. Failing closed is essential for
            // a high-risk operation: another client may have marked a
            // different message `\\Deleted` in the same mailbox.
            return Err(ApplyError::Unsupported);
        }
        for (folder, uids) in group_by_folder(&messages) {
            self.ensure_selected(&folder)?;
            self.session
                .uid_store_flag(&uids, "\\Deleted", true)
                .map_err(connect_to_apply)?;
            self.session.uid_expunge(&uids).map_err(connect_to_apply)?;
            self.selected = None;
        }
        Ok(())
    }

    fn create_folder(&mut self, name: &str) -> Result<(), ApplyError> {
        match self.session.create_mailbox(name) {
            Ok(()) => Ok(()),
            Err(ConnectError::Network { details, .. }) if is_already_exists(&details) => Ok(()),
            Err(err) => Err(connect_to_apply(err)),
        }
    }

    /// T-060u. Three things have to be true before `DELETE` goes out, and
    /// none of them can be assumed from the local mirror:
    ///
    /// 1. **The mailbox is empty *on the server*.** Core refuses to queue a
    ///    deletion for a folder it can see mail in, but the queue is
    ///    asynchronous: a message can arrive between that check and this
    ///    call, and `DELETE` on most servers takes the mail with it. So the
    ///    `SELECT` here is not a redundant second check, it is the only one
    ///    that happens at the moment of destruction. A non-empty mailbox
    ///    fails terminally ([`ApplyError::NotEmpty`]) rather than retrying:
    ///    retrying would just mean deleting the mail later instead of now.
    /// 2. **The mailbox is not the selected one.** RFC 3501 leaves
    ///    `DELETE` of the selected mailbox implementation-defined, so the
    ///    session steps onto `INBOX` first.
    /// 3. **A mailbox that is already gone is success, not failure** (D29).
    ///    The operation asked for the folder to not exist; it does not.
    ///    Both the `SELECT` and the `DELETE` can be the call that discovers
    ///    this, because another client may have removed it at any point.
    fn delete_folder(&mut self, mailbox: &str) -> Result<(), ApplyError> {
        match self.session.select(mailbox) {
            Ok(selected) => {
                self.selected = Some(mailbox.to_string());
                if selected.exists > 0 {
                    return Err(ApplyError::NotEmpty);
                }
            }
            Err(ConnectError::Network { details, .. }) if is_missing_mailbox(&details) => {
                self.selected = None;
                return Ok(());
            }
            Err(err) => {
                self.selected = None;
                return Err(connect_to_apply(err));
            }
        }
        self.ensure_selected(INBOX_MAILBOX)?;
        match self.session.delete_mailbox(mailbox) {
            Ok(()) => Ok(()),
            Err(ConnectError::Network { details, .. }) if is_missing_mailbox(&details) => Ok(()),
            Err(err) => Err(connect_to_apply(err)),
        }
    }
}

impl<L: RemoteLocator> MailProvider for ImapMailProvider<L> {
    fn apply(&mut self, op: &Operation) -> Result<(), ApplyError> {
        match op.kind {
            OpKind::MarkRead => self.store_flag(&op.account_id, &op.target_id, "\\Seen", true),
            OpKind::MarkUnread => self.store_flag(&op.account_id, &op.target_id, "\\Seen", false),
            OpKind::Star => self.store_flag(&op.account_id, &op.target_id, "\\Flagged", true),
            OpKind::Unstar => self.store_flag(&op.account_id, &op.target_id, "\\Flagged", false),
            OpKind::Trash => self.move_thread(
                &op.account_id,
                &op.target_id,
                TRASH_FOLDER_KEY,
                op.id.as_str(),
            ),
            OpKind::PermanentDelete => {
                self.permanently_delete_thread(&op.account_id, &op.target_id)
            }
            OpKind::Archive => self.move_thread(
                &op.account_id,
                &op.target_id,
                ARCHIVE_FOLDER_KEY,
                op.id.as_str(),
            ),
            OpKind::Move => {
                let folder_id =
                    json_string(&op.payload, "folder_id").ok_or(ApplyError::Unsupported)?;
                self.move_thread(&op.account_id, &op.target_id, &folder_id, op.id.as_str())
            }
            OpKind::CreateFolder => {
                let name = json_string(&op.payload, "name").ok_or(ApplyError::Unsupported)?;
                self.create_folder(&name)
            }
            OpKind::DeleteFolder => {
                // The mailbox path comes from Core's `folders.remote_id`:
                // the verbatim name the server itself reported in `LIST`.
                let mailbox = json_string(&op.payload, "mailbox").ok_or(ApplyError::Unsupported)?;
                self.delete_folder(&mailbox)
            }
            OpKind::RenameFolder => {
                // Both paths come from Core, which owns folder identity and
                // the stored delimiter. Rebuilding either one here would be
                // a second, drifting source of truth for where the mailbox
                // lives.
                let from = json_string(&op.payload, "from").ok_or(ApplyError::Unsupported)?;
                let to = json_string(&op.payload, "to").ok_or(ApplyError::Unsupported)?;
                self.session
                    .rename_mailbox(&from, &to)
                    .map_err(connect_to_apply)
            }
            // Snooze is a client-local overlay with no IMAP representation
            // (D-scope, T-025): nothing to send, so it always acks.
            OpKind::Snooze => Ok(()),
            // Sending mail is SMTP/outbox territory (a different worker,
            // out of T-025's scope), not something this IMAP applier does.
            OpKind::Send => Err(ApplyError::Unsupported),
            // T-042 is routed by feathermail-service's composition-root
            // adapter: it loads the durable draft body from Core and calls
            // MailSession::append_draft. This low-level thread-operation
            // provider deliberately never reads draft content itself.
            OpKind::SyncDraft => Err(ApplyError::Unsupported),
        }
    }
}

/// `pub(crate)` (not private) so `crate::oauth::OauthReauth` (T-083) can
/// classify the `ConnectError` from its own reconnect the same way a normal
/// `apply()` call does -- one mapping, not two that could drift apart.
pub(crate) fn connect_to_apply(err: ConnectError) -> ApplyError {
    match err {
        ConnectError::Auth { .. } => ApplyError::Auth,
        ConnectError::Network { .. } => ApplyError::Network,
        ConnectError::Invalid { .. } => ApplyError::Unsupported,
    }
}

fn is_already_exists(details: &Option<String>) -> bool {
    let Some(details) = details else {
        return false;
    };
    let lower = details.to_ascii_lowercase();
    lower.contains("already exist") || lower.contains("mailbox exists")
}

/// T-060u, and the same shape as [`is_already_exists`] one line up: a
/// tagged `NO` carries its reason only as prose plus an optional response
/// code, and `ImapSession` flattens both into `ConnectError::Network`'s
/// `details` (D14 -- already scrubbed of anything secret). Sniffing that
/// text is not elegant, but the alternative is a `ConnectError` variant
/// per IMAP response code, and the caller here needs exactly one bit:
/// "gone already" (success for a deletion) or "something else" (a real
/// failure). `[NONEXISTENT]` is RFC 5530's machine-readable form; the
/// other two phrasings cover servers that never adopted it.
fn is_missing_mailbox(details: &Option<String>) -> bool {
    let Some(details) = details else {
        return false;
    };
    let lower = details.to_ascii_lowercase();
    lower.contains("nonexistent")
        || lower.contains("no such mailbox")
        || lower.contains("does not exist")
}

fn group_by_folder(messages: &[RemoteMessage]) -> Vec<(String, Vec<u32>)> {
    let mut groups: Vec<(String, Vec<u32>)> = Vec::new();
    for m in messages {
        match groups.iter_mut().find(|(folder, _)| folder == &m.folder) {
            Some(entry) => entry.1.push(m.uid),
            None => groups.push((m.folder.clone(), vec![m.uid])),
        }
    }
    groups
}

/// Minimal `"key":"value"` extractor for the small, flat JSON payloads
/// `feathermail_core::store` writes (see its `json_escape`: only `\` and
/// `"` are ever escaped). Not a general JSON parser — this crate has no
/// JSON dependency, and none of this module's payloads need one.
fn json_string(payload: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = payload.find(&needle)? + needle.len();
    let mut out = String::new();
    let mut chars = payload[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(out),
            _ => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_core::command::Command;
    use feathermail_core::model::{FolderId, OpStatus, OperationId, ThreadId, FIXTURE_NOW};
    use feathermail_core::queue::TickOutcome;
    use feathermail_core::store::Core;
    use feathermail_core::{ConnectOk, MailConnector, MailSecurity, MailboxForm};
    use std::collections::HashMap;
    use std::thread;
    use std::time::Duration;

    use crate::session::ImapAuth;
    // T-082 moved the fake IMAP server out to `crate::test_support` (a
    // shared module, also reachable from `crates/service`'s tests behind
    // the `test-support` feature) so it isn't duplicated between there and
    // here. Aliased on import so none of this module's own call sites had
    // to change.
    use crate::test_support::spawn_fake_imap_server as spawn_fake_server;

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

    fn connect(port: u16) -> ImapSession {
        thread::sleep(Duration::from_millis(30));
        ImapSession::connect(&form(port), ImapAuth::Login("x".into())).unwrap()
    }

    // --- Fake RemoteLocator ---

    #[derive(Default)]
    struct FakeLocator {
        messages: HashMap<(String, String), Vec<RemoteMessage>>,
        folders: HashMap<(String, String), String>,
        operation_messages: HashMap<(String, String, String), Vec<RemoteMessage>>,
        operation_folders: HashMap<(String, String, String), String>,
    }

    impl FakeLocator {
        fn with_thread(account: &str, thread: &str, messages: Vec<RemoteMessage>) -> Self {
            let mut s = Self::default();
            s.messages
                .insert((account.to_string(), thread.to_string()), messages);
            s
        }

        fn folder(mut self, account: &str, key: &str, remote: &str) -> Self {
            self.folders
                .insert((account.to_string(), key.to_string()), remote.to_string());
            self
        }

        fn operation_move(
            mut self,
            account: &str,
            operation_id: &str,
            source: RemoteMessage,
            destination: &str,
        ) -> Self {
            self.operation_messages.insert(
                (
                    account.to_string(),
                    "t1".to_string(),
                    operation_id.to_string(),
                ),
                vec![source],
            );
            self.operation_folders.insert(
                (
                    account.to_string(),
                    ARCHIVE_FOLDER_KEY.to_string(),
                    operation_id.to_string(),
                ),
                destination.to_string(),
            );
            self
        }
    }

    impl RemoteLocator for FakeLocator {
        fn thread_messages(
            &self,
            account_id: &AccountId,
            thread_id: &str,
        ) -> Result<Vec<RemoteMessage>, ApplyError> {
            self.messages
                .get(&(account_id.as_str().to_string(), thread_id.to_string()))
                .cloned()
                .ok_or(ApplyError::NotFound)
        }

        fn remote_folder(
            &self,
            account_id: &AccountId,
            folder_key: &str,
        ) -> Result<String, ApplyError> {
            self.folders
                .get(&(account_id.as_str().to_string(), folder_key.to_string()))
                .cloned()
                .ok_or(ApplyError::Unsupported)
        }

        fn thread_messages_for_operation(
            &self,
            account_id: &AccountId,
            thread_id: &str,
            operation_id: &str,
        ) -> Result<Vec<RemoteMessage>, ApplyError> {
            self.operation_messages
                .get(&(
                    account_id.as_str().to_string(),
                    thread_id.to_string(),
                    operation_id.to_string(),
                ))
                .cloned()
                .or_else(|| {
                    self.messages
                        .get(&(account_id.as_str().to_string(), thread_id.to_string()))
                        .cloned()
                })
                .ok_or(ApplyError::NotFound)
        }

        fn remote_folder_for_operation(
            &self,
            account_id: &AccountId,
            folder_key: &str,
            operation_id: &str,
        ) -> Result<String, ApplyError> {
            self.operation_folders
                .get(&(
                    account_id.as_str().to_string(),
                    folder_key.to_string(),
                    operation_id.to_string(),
                ))
                .cloned()
                .or_else(|| {
                    self.folders
                        .get(&(account_id.as_str().to_string(), folder_key.to_string()))
                        .cloned()
                })
                .ok_or(ApplyError::Unsupported)
        }
    }

    fn op(kind: OpKind, target: &str, payload: &str) -> Operation {
        Operation {
            id: OperationId(format!("{}:{target}", kind.as_str())),
            account_id: AccountId("john".into()),
            target_id: target.into(),
            kind,
            payload: payload.into(),
            payload_hash: "hash".into(),
            created_at: FIXTURE_NOW,
            retry_count: 0,
            next_attempt_at: None,
            status: OpStatus::Pending,
            undo_of: None,
        }
    }

    // --- Flags ---

    #[test]
    fn mark_read_stores_seen_flag_on_server() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![7])], true);
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        );
        let mut provider = ImapMailProvider::new(session, locator);
        provider
            .apply(&op(OpKind::MarkRead, "t1", r#"{"read":true}"#))
            .unwrap();

        let st = state.lock().unwrap();
        let msg = st.mailboxes["INBOX"].iter().find(|m| m.uid == 7).unwrap();
        assert!(
            msg.flags.iter().any(|f| f == "\\Seen"),
            "server must see \\Seen, has {:?}",
            msg.flags
        );
    }

    #[test]
    fn mark_unread_removes_seen_flag_on_server() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![7])], true);
        {
            let mut st = state.lock().unwrap();
            st.mailboxes.get_mut("INBOX").unwrap()[0]
                .flags
                .push("\\Seen".into());
        }
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        );
        let mut provider = ImapMailProvider::new(session, locator);
        provider
            .apply(&op(OpKind::MarkUnread, "t1", r#"{"read":false}"#))
            .unwrap();

        let st = state.lock().unwrap();
        let msg = st.mailboxes["INBOX"].iter().find(|m| m.uid == 7).unwrap();
        assert!(!msg.flags.iter().any(|f| f == "\\Seen"));
    }

    #[test]
    fn star_stores_flagged_flag_on_server() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![7])], true);
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        );
        let mut provider = ImapMailProvider::new(session, locator);
        provider
            .apply(&op(OpKind::Star, "t1", r#"{"starred":true}"#))
            .unwrap();

        let st = state.lock().unwrap();
        let msg = st.mailboxes["INBOX"].iter().find(|m| m.uid == 7).unwrap();
        assert!(msg.flags.iter().any(|f| f == "\\Flagged"));
    }

    #[test]
    fn trash_moves_message_to_trash_folder_with_move_capability() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![7]), ("Trash", vec![])], true);
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        )
        .folder("john", TRASH_FOLDER_KEY, "Trash");
        let mut provider = ImapMailProvider::new(session, locator);
        provider.apply(&op(OpKind::Trash, "t1", "{}")).unwrap();

        let st = state.lock().unwrap();
        assert!(
            st.mailboxes["INBOX"].is_empty(),
            "trashed message must leave INBOX, like Archive does — a \
             bare flag would leave it visible to every other client"
        );
        assert_eq!(
            st.mailboxes["Trash"].len(),
            1,
            "server must see the message land in Trash"
        );
    }

    #[test]
    fn trash_falls_back_to_copy_store_expunge_without_move_capability() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![7]), ("Trash", vec![])], false);
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        )
        .folder("john", TRASH_FOLDER_KEY, "Trash");
        let mut provider = ImapMailProvider::new(session, locator);
        provider.apply(&op(OpKind::Trash, "t1", "{}")).unwrap();

        let st = state.lock().unwrap();
        assert!(
            st.mailboxes["INBOX"].is_empty(),
            "fallback must still remove the message from the source folder"
        );
        assert_eq!(
            st.mailboxes["Trash"].len(),
            1,
            "fallback must still land the message in Trash"
        );
    }

    #[test]
    fn permanent_delete_expunge_bypasses_trash_and_preserves_unrelated_deleted_uid() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![7, 8]), ("Trash", vec![])], true);
        state
            .lock()
            .unwrap()
            .mailboxes
            .get_mut("INBOX")
            .unwrap()
            .iter_mut()
            .find(|m| m.uid == 8)
            .unwrap()
            .flags
            .push("\\Deleted".into());
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        );
        let mut provider = ImapMailProvider::new(session, locator);
        provider
            .apply(&op(OpKind::PermanentDelete, "t1", "{}"))
            .unwrap();

        let st = state.lock().unwrap();
        assert_eq!(
            st.mailboxes["INBOX"]
                .iter()
                .map(|m| m.uid)
                .collect::<Vec<_>>(),
            vec![8],
            "UID EXPUNGE must not remove another message already marked Deleted"
        );
        assert!(
            st.mailboxes["Trash"].is_empty(),
            "permanent delete must never move a message into Trash"
        );
    }

    #[test]
    fn trash_without_resolvable_trash_folder_is_an_error_not_silently_applied() {
        // No `.folder("john", TRASH_FOLDER_KEY, ...)` configured: the
        // locator cannot resolve a Trash mailbox (e.g. the server never
        // advertised \Trash via LIST SPECIAL-USE). The op must fail loudly
        // — and stay queued for retry — rather than inventing a mailbox
        // name and quietly doing nothing useful to it.
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![7])], true);
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        );
        let mut provider = ImapMailProvider::new(session, locator);
        let err = provider.apply(&op(OpKind::Trash, "t1", "{}")).unwrap_err();
        assert_eq!(err, ApplyError::Unsupported);

        let st = state.lock().unwrap();
        assert_eq!(
            st.mailboxes["INBOX"].len(),
            1,
            "message must be untouched on the server when Trash can't be resolved"
        );
    }

    // --- Move / Archive ---

    #[test]
    fn archive_moves_message_to_archive_folder_with_move_capability() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![7]), ("Archive", vec![])], true);
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        )
        .folder("john", ARCHIVE_FOLDER_KEY, "Archive");
        let mut provider = ImapMailProvider::new(session, locator);
        provider.apply(&op(OpKind::Archive, "t1", "{}")).unwrap();

        let st = state.lock().unwrap();
        assert!(
            st.mailboxes["INBOX"].is_empty(),
            "message must have left INBOX"
        );
        assert_eq!(
            st.mailboxes["Archive"].len(),
            1,
            "server must see the archived message"
        );
    }

    #[test]
    fn move_uses_the_captured_operation_source_and_destination() {
        let (port, state) = spawn_fake_server(
            vec![
                ("INBOX", vec![7]),
                ("CapturedSource", vec![9]),
                ("Archive", vec![]),
                ("CapturedArchive", vec![]),
            ],
            true,
        );
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        )
        .folder("john", ARCHIVE_FOLDER_KEY, "Archive")
        .operation_move(
            "john",
            "archive:t1",
            RemoteMessage {
                folder: "CapturedSource".into(),
                uid: 9,
            },
            "CapturedArchive",
        );
        let mut provider = ImapMailProvider::new(session, locator);
        provider.apply(&op(OpKind::Archive, "t1", "{}")).unwrap();

        let st = state.lock().unwrap();
        assert_eq!(st.mailboxes["INBOX"].len(), 1);
        assert_eq!(st.mailboxes["CapturedSource"].len(), 0);
        assert_eq!(st.mailboxes["Archive"].len(), 0);
        assert_eq!(st.mailboxes["CapturedArchive"].len(), 1);
    }

    #[test]
    fn archive_falls_back_to_copy_store_expunge_without_move_capability() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![7]), ("Archive", vec![])], false);
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        )
        .folder("john", ARCHIVE_FOLDER_KEY, "Archive");
        let mut provider = ImapMailProvider::new(session, locator);
        provider.apply(&op(OpKind::Archive, "t1", "{}")).unwrap();

        let st = state.lock().unwrap();
        assert!(
            st.mailboxes["INBOX"].is_empty(),
            "fallback must still remove the message from the source folder"
        );
        assert_eq!(
            st.mailboxes["Archive"].len(),
            1,
            "fallback must still land the message in Archive"
        );
    }

    #[test]
    fn move_to_custom_folder_reaches_server() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![7]), ("Projects", vec![])], true);
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
        )
        .folder("john", "john:projects", "Projects");
        let mut provider = ImapMailProvider::new(session, locator);
        provider
            .apply(&op(OpKind::Move, "t1", r#"{"folder_id":"john:projects"}"#))
            .unwrap();

        let st = state.lock().unwrap();
        assert!(st.mailboxes["INBOX"].is_empty());
        assert_eq!(st.mailboxes["Projects"].len(), 1);
    }

    // --- Folder create ---

    #[test]
    fn create_folder_reaches_server() {
        let (port, state) = spawn_fake_server(vec![], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        provider
            .apply(&op(
                OpKind::CreateFolder,
                "john:projects",
                r#"{"name":"Projects"}"#,
            ))
            .unwrap();

        assert!(state.lock().unwrap().mailboxes.contains_key("Projects"));
    }

    #[test]
    fn create_folder_already_existing_is_idempotent() {
        let (port, _state) = spawn_fake_server(vec![("Projects", vec![])], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        // Must not surface as an error: same name already there is D29's
        // idempotent no-op, not a conflict the worker should fail on.
        provider
            .apply(&op(
                OpKind::CreateFolder,
                "john:projects",
                r#"{"name":"Projects"}"#,
            ))
            .unwrap();
    }

    // --- Folder rename ---

    #[test]
    fn rename_folder_moves_the_mailbox_and_its_mail() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![1]), ("Ideas", vec![9])], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        provider
            .apply(&op(
                OpKind::RenameFolder,
                "john:ideas",
                r#"{"from":"Ideas","to":"Plans","at":1000}"#,
            ))
            .unwrap();

        let st = state.lock().unwrap();
        assert!(!st.mailboxes.contains_key("Ideas"));
        assert_eq!(
            st.mailboxes["Plans"].len(),
            1,
            "a rename must carry the mail with the name"
        );
    }

    #[test]
    fn rename_folder_keeps_a_nested_path_the_applier_never_builds_itself() {
        let (port, state) =
            spawn_fake_server(vec![("INBOX", vec![1]), ("Team/Ideas", vec![9])], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        // Core computed both halves; the applier only relays them. That is
        // the whole reason `to` is in the payload rather than a bare leaf.
        provider
            .apply(&op(
                OpKind::RenameFolder,
                "john:ideas",
                r#"{"from":"Team/Ideas","to":"Team/Plans","at":1000}"#,
            ))
            .unwrap();

        let st = state.lock().unwrap();
        assert!(!st.mailboxes.contains_key("Team/Ideas"));
        assert!(st.mailboxes.contains_key("Team/Plans"));
        assert!(
            !st.mailboxes.contains_key("Plans"),
            "the folder must not be promoted out of its hierarchy"
        );
    }

    #[test]
    fn rename_folder_onto_an_existing_mailbox_fails_instead_of_merging() {
        let (port, state) = spawn_fake_server(vec![("Ideas", vec![9]), ("Plans", vec![4])], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        // Core's duplicate check runs against the local mirror, which can be
        // stale. When the server says no, that must reach the queue as a
        // failure -- silently accepting it would strand `Ideas`'s mail.
        provider
            .apply(&op(
                OpKind::RenameFolder,
                "john:ideas",
                r#"{"from":"Ideas","to":"Plans","at":1000}"#,
            ))
            .unwrap_err();

        let st = state.lock().unwrap();
        assert_eq!(st.mailboxes["Ideas"].len(), 1);
        assert_eq!(st.mailboxes["Plans"].len(), 1);
    }

    #[test]
    fn rename_folder_payload_without_both_names_is_unsupported_not_a_panic() {
        let (port, _state) = spawn_fake_server(vec![("Ideas", vec![])], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        for payload in [r#"{}"#, r#"{"from":"Ideas"}"#, r#"{"to":"Plans"}"#] {
            let err = provider
                .apply(&op(OpKind::RenameFolder, "john:ideas", payload))
                .unwrap_err();
            assert_eq!(err, ApplyError::Unsupported, "{payload}");
        }
    }

    // --- Folder delete ---

    #[test]
    fn delete_folder_removes_an_empty_mailbox() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![1]), ("Ideas", vec![])], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        provider
            .apply(&op(
                OpKind::DeleteFolder,
                "john:ideas",
                r#"{"mailbox":"Ideas","at":1000}"#,
            ))
            .unwrap();

        let st = state.lock().unwrap();
        assert!(!st.mailboxes.contains_key("Ideas"));
        assert_eq!(
            st.mailboxes["INBOX"].len(),
            1,
            "stepping off the target must not disturb the mailbox stepped onto"
        );
    }

    /// The whole reason [`ImapMailProvider::delete_folder`] pays for a
    /// `SELECT`. Core refused to queue this while the folder looked empty
    /// locally; by the time the queue drained, a message had landed. This
    /// fake server is the permissive kind that would happily take the mail
    /// down with the mailbox, so the mail surviving proves the guard ran.
    #[test]
    fn delete_folder_refuses_a_mailbox_that_filled_up_after_the_local_check() {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![1]), ("Ideas", vec![9])], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        let err = provider
            .apply(&op(
                OpKind::DeleteFolder,
                "john:ideas",
                r#"{"mailbox":"Ideas","at":1000}"#,
            ))
            .unwrap_err();

        assert_eq!(err, ApplyError::NotEmpty);
        assert!(!err.retry(), "waiting will not make the mail disappear");
        let st = state.lock().unwrap();
        assert_eq!(st.mailboxes["Ideas"].len(), 1);
    }

    #[test]
    fn delete_folder_keeps_a_nested_path_the_applier_never_builds_itself() {
        let (port, state) =
            spawn_fake_server(vec![("INBOX", vec![1]), ("Team/Ideas", vec![])], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        provider
            .apply(&op(
                OpKind::DeleteFolder,
                "john:ideas",
                r#"{"mailbox":"Team/Ideas","at":1000}"#,
            ))
            .unwrap();

        let st = state.lock().unwrap();
        assert!(!st.mailboxes.contains_key("Team/Ideas"));
        assert!(
            !st.mailboxes.contains_key("Ideas"),
            "the leaf name alone must never be what gets deleted"
        );
    }

    #[test]
    fn delete_folder_payload_without_a_mailbox_is_unsupported_not_a_panic() {
        let (port, _state) = spawn_fake_server(vec![("Ideas", vec![])], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        let err = provider
            .apply(&op(OpKind::DeleteFolder, "john:ideas", r#"{"at":1000}"#))
            .unwrap_err();
        assert_eq!(err, ApplyError::Unsupported);
    }

    /// `is_missing_mailbox` decides whether a deletion that found nothing
    /// to delete counts as done (D29) or as a failure, and it decides it
    /// from prose. Pin both directions: the RFC 5530 response code and the
    /// two common phrasings must pass, and nothing else may -- a `NO` that
    /// means "permission denied" must not be read as "already gone".
    #[test]
    fn missing_mailbox_is_recognised_only_from_the_phrasings_that_mean_it() {
        for details in [
            "[NONEXISTENT] No such mailbox",
            "no such mailbox",
            "Mailbox does not exist",
        ] {
            assert!(is_missing_mailbox(&Some(details.to_string())), "{details}");
        }
        for details in [
            "[NOPERM] Permission denied",
            "Mailbox is not empty",
            "Over quota",
        ] {
            assert!(!is_missing_mailbox(&Some(details.to_string())), "{details}");
        }
        assert!(!is_missing_mailbox(&None));
    }

    // --- Full offline-then-online path through the real queue ---

    struct NoProbeConnector;

    impl MailConnector for NoProbeConnector {
        fn probe(
            &self,
            _form: &MailboxForm,
            _password: &str,
        ) -> Result<ConnectOk, feathermail_core::ConnectError> {
            Ok(ConnectOk {
                capabilities: Vec::new(),
            })
        }
    }

    /// The key scenario (T-025): a mutation is queued with no server
    /// connection at all — `Core::create_folder` only ever touches SQLite
    /// — and only once that is done does a connection to the server come
    /// up. `Core::tick` must then find the still-pending operation and
    /// this provider's `apply` must make the fake server see it, proving
    /// the queue → provider → wire path end to end (not just this
    /// module's own `apply` calls in isolation, like the tests above).
    #[test]
    fn folder_queued_offline_then_applied_once_online_server_sees_it() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        let account_id = core
            .add_account(
                &MailboxForm {
                    email: "john@example.com".into(),
                    imap_host: "imap.example.com".into(),
                    imap_port: 993,
                    imap_security: MailSecurity::Ssl,
                    smtp_host: "smtp.example.com".into(),
                    smtp_port: 465,
                    smtp_security: MailSecurity::Ssl,
                },
                "hunter2",
                &NoProbeConnector,
            )
            .unwrap();

        // Offline: this is pure SQLite, no socket anywhere yet.
        core.create_folder(&account_id, "Projects").unwrap();

        // The connection "appears" only now.
        let (port, state) = spawn_fake_server(vec![], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());

        let outcome = core.tick(&mut provider).unwrap();
        assert!(
            matches!(outcome, TickOutcome::Acked(_)),
            "expected the queued op to be acked, got {outcome:?}"
        );
        assert!(
            state.lock().unwrap().mailboxes.contains_key("Projects"),
            "the fake server must see the folder the worker created"
        );
    }

    #[test]
    fn move_without_thread_messages_is_not_found() {
        let (port, _state) = spawn_fake_server(vec![], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        let err = provider
            .apply(&op(OpKind::Archive, "missing-thread", "{}"))
            .unwrap_err();
        assert_eq!(err, ApplyError::NotFound);
    }

    #[test]
    fn create_folder_payload_missing_name_is_unsupported_not_a_panic() {
        let (port, _state) = spawn_fake_server(vec![], true);
        let session = connect(port);
        let mut provider = ImapMailProvider::new(session, FakeLocator::default());
        let err = provider
            .apply(&op(OpKind::CreateFolder, "john:x", "{}"))
            .unwrap_err();
        assert_eq!(err, ApplyError::Unsupported);
    }

    #[test]
    fn json_string_reads_escaped_quotes_and_backslashes() {
        assert_eq!(
            json_string(r#"{"name":"a\"b\\c"}"#, "name").as_deref(),
            Some("a\"b\\c")
        );
        assert_eq!(json_string(r#"{"other":"x"}"#, "name"), None);
    }

    // FolderId import above is only used to spell out the doc comment
    // example in prose; keep the compiler happy without a spurious import.
    #[allow(dead_code)]
    fn _unused(_: FolderId) {}
    #[allow(dead_code)]
    fn _unused_cmd(_: Command) {}
    #[allow(dead_code)]
    fn _unused_thread(_: ThreadId) {}

    // --- The `mailbox()` cache-invalidation trap (T-078 (b)) ---

    /// Proves `mailbox()` invalidating `selected` on every call is load-
    /// bearing, not defensive over-caution -- by observable wire behavior
    /// (which mailbox's message actually changed), per this method's own
    /// doc comment. `INBOX` and `OTHER` deliberately share UID 5, so if a
    /// `STORE` ever lands on the wrong one, it is visible as a flag
    /// appearing on the wrong mailbox's message rather than merely
    /// vanishing silently.
    ///
    /// Sequence: `apply` a queued op against `INBOX` (caches
    /// `selected = Some("INBOX")`), then reach past `ImapMailProvider`
    /// entirely and call `mailbox().select("OTHER")` directly -- exactly
    /// what `feathermail_sync::sync_folder` does mid-pass, bypassing
    /// `ensure_selected` -- then `apply` a second queued op that, per the
    /// locator, still targets `INBOX`.
    ///
    /// If `mailbox()` did not reset `selected`, `ensure_selected("INBOX")`
    /// would see its stale cached value already matching, skip re-issuing
    /// `SELECT`, and the second op's `STORE` would run against whatever
    /// the wire actually has selected -- `OTHER` -- flipping the wrong
    /// mailbox's message and leaving `INBOX`'s message unchanged.
    #[test]
    fn mailbox_accessor_invalidates_the_selected_cache_so_the_next_apply_targets_the_right_mailbox()
    {
        let (port, state) = spawn_fake_server(vec![("INBOX", vec![5]), ("OTHER", vec![5])], true);
        let session = connect(port);
        let locator = FakeLocator::with_thread(
            "john",
            "t1",
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 5,
            }],
        );
        let mut provider = ImapMailProvider::new(session, locator);

        // 1. A normal queued op against INBOX -- caches `selected =
        //    Some("INBOX")` inside the provider.
        provider
            .apply(&op(OpKind::MarkRead, "t1", r#"{"read":true}"#))
            .unwrap();

        // 2. Out-of-band, exactly like `sync_folder` mid-pass: select a
        //    *different* mailbox directly through the session, with no
        //    way for `ImapMailProvider` to find out other than through
        //    `mailbox()`'s own reset.
        provider.mailbox().select("OTHER").unwrap();

        // 3. A second queued op that, per the locator, still targets
        //    INBOX -- must re-select INBOX rather than trust the stale
        //    cache.
        provider
            .apply(&op(OpKind::Star, "t1", r#"{"starred":true}"#))
            .unwrap();

        let st = state.lock().unwrap();
        let inbox_msg = st.mailboxes["INBOX"].iter().find(|m| m.uid == 5).unwrap();
        assert!(
            inbox_msg.flags.iter().any(|f| f == "\\Seen"),
            "INBOX/5 must still carry \\Seen from step 1, has {:?}",
            inbox_msg.flags
        );
        assert!(
            inbox_msg.flags.iter().any(|f| f == "\\Flagged"),
            "INBOX/5 must have received \\Flagged from step 3 -- got {:?}; \
             if this is missing, the STORE in step 3 landed on the wrong \
             mailbox because the stale `selected` cache skipped SELECT",
            inbox_msg.flags
        );

        let other_msg = st.mailboxes["OTHER"].iter().find(|m| m.uid == 5).unwrap();
        assert!(
            other_msg.flags.is_empty(),
            "OTHER/5 must be untouched by step 3's op, has {:?} -- a \
             non-empty flag set here means step 3's STORE ran against \
             OTHER instead of INBOX",
            other_msg.flags
        );
    }
}
