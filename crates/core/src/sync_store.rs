//! `impl SyncStore for CoreSyncStore` -- T-022 second half, the SQLite side
//! of the sync engine's world.
//!
//! This lives here, in `feathermail-core`, rather than in
//! `feathermail-sync` itself or in `feathermail-db`, on purpose:
//! `feathermail_sync::SyncStore` is a foreign trait here, and
//! `feathermail_db::Database` is a foreign type here too -- neither alone
//! satisfies Rust's orphan rule (an `impl` needs either the trait or the
//! type to be local to the crate writing it). [`CoreSyncStore`] is a new
//! type this crate defines, wrapping a borrowed `&Database`, which *is*
//! local, so implementing the foreign `SyncStore` trait for it is legal.
//! This crate already owns `Database` (T-007); it is also the one place in
//! the workspace everything is meant to go through (D9), which is exactly
//! what will let `Core` (not UI, not MCP) drive a sync pass later. The IMAP
//! side of this same split ([`feathermail_sync::MailboxSession`]) lives in
//! `feathermail-providers`, which owns `ImapSession`, for the identical
//! reason. `feathermail-sync` itself stays free of any concrete dependency:
//! pure trait + pure logic.
//!
//! [`CoreSyncStore`] is bound to one `(account_id, folder_id)` pair at
//! construction. The `folder` argument every [`SyncStore`] method receives
//! is the engine's IMAP folder *name* (what the paired `MailboxSession`
//! uses for `SELECT`) -- not this database's `folders.id` -- so it is
//! intentionally ignored here; storage identity comes from the
//! account_id/folder_id fixed at construction, not re-derived from that
//! string.
//!
//! ## Thread assignment (T-029, D22)
//! `messages.thread_id` is `NOT NULL` with a FK into `threads`, so a brand-
//! new UID still gets a candidate `threads` row (`thr:{acct}:{folder}:{uid}`)
//! so the insert can land. After the batch — still inside the same
//! transaction — [`CoreSyncStore::assign_and_rollup`] loads the whole
//! folder's headers and runs [`crate::threading::assign_groups`]: Gmail
//! `X-GM-THRID` beats JWZ, grouping is folder-local, subject is ignored.
//! Surviving `threads.id` is the lexicographically smallest existing id in
//! the group (the one already sitting on an anchor row). Rollup then
//! rewrites `message_count`, latest subject/date/snippet, and
//! unread/starred/has_attachment as OR across members of the **affected**
//! thread ids (the batch, remapped through retarget, ∪ survivors). Absorbed
//! ids are empty after messages move and are deleted, not rolled up. Never
//! every thread in the folder — otherwise a flags-only CONDSTORE of one
//! UID, or an empty `upsert_headers` the engine still calls, would clobber
//! an optimistic MarkRead on a neighbour (T-028). Empty absorbed threads are deleted;
//! pending `operations` / `snoozes` / `drafts` that pointed at an
//! absorbed id are retargeted. A one-shot
//! [`CoreSyncStore::rethread_folder`] on [`crate::Core::open`] copies
//! `threads.unread`/`starred` down onto members first (live profiles
//! already marked read at thread level), then regroups; that path may
//! roll up the whole folder because it is one-shot, not the hot
//! CONDSTORE.
//!
//! ## `upsert_headers` merge semantics
//! [`feathermail_sync::HeaderMeta`] doc comments require a flags-only
//! update (every field but `uid`/`flags` left at its Rust default) to merge
//! into an existing row rather than clobber it. Concretely: `flags` (and
//! the `unread`/`starred` derived from it) always overwrite, because both a
//! full fetch and a flags-only fetch always carry real flags; every other
//! column is only overwritten when the incoming value is genuinely present
//! (`Some(..)`, or a non-empty `references` list) via SQL `COALESCE`
//! against the previously stored value, so a `None` from a flags-only
//! fetch never blanks out a subject/sender/date a full fetch already wrote.

use std::collections::{HashMap, HashSet};

use feathermail_db::Database;
use feathermail_html::decode_encoded_words;
use feathermail_sync::schedule::{FolderInput, FolderRole};
use feathermail_sync::{FolderSyncState, HeaderMeta, SyncError, SyncStore};
use rusqlite::{params, OptionalExtension};

use crate::error::CoreError;
use crate::model::{AccountId, FolderKind};
use crate::store::{sql_err, Core};
use crate::threading::{assign_groups, ThreadHint};

/// Subject shown for a brand-new message whose headers carried nothing
/// usable at all (T-027 -- see `upsert_one`'s insert branch). Human copy,
/// no protocol jargon, and never derived from the message's own (unparsed,
/// untrusted) bytes.
const UNPARSEABLE_SUBJECT: &str = "Unable to display";

/// See the module docs above: an adapter over `&Database`, bound to one
/// `(account_id, folder_id)` pair.
pub struct CoreSyncStore<'a> {
    db: &'a Database,
    account_id: String,
    folder_id: String,
}

struct ExistingMessage {
    id: String,
    thread_id: String,
}

struct MoveCandidate {
    operation_id: String,
    message_id: String,
    destination_uid: Option<i64>,
    thread_id: String,
    message_id_header: Option<String>,
    date: i64,
    size_bytes: i64,
}

struct MoveIntent {
    operation_id: String,
    message_id: String,
    thread_id: String,
}

impl<'a> CoreSyncStore<'a> {
    pub fn new(
        db: &'a Database,
        account_id: impl Into<String>,
        folder_id: impl Into<String>,
    ) -> Self {
        Self {
            db,
            account_id: account_id.into(),
            folder_id: folder_id.into(),
        }
    }

    fn message_row_id(&self, uid: u32) -> String {
        format!("msg:{}:{}:{}", self.account_id, self.folder_id, uid)
    }

    fn thread_row_id(&self, uid: u32) -> String {
        format!("thr:{}:{}:{}", self.account_id, self.folder_id, uid)
    }

    fn existing_message(&self, id: &str) -> Result<Option<ExistingMessage>, SyncError> {
        self.db
            .conn()
            .query_row(
                "SELECT id, thread_id, folder_id, provider_uid
                 FROM messages WHERE account_id = ?1 AND id = ?2",
                params![self.account_id, id],
                |row| {
                    Ok(ExistingMessage {
                        id: row.get(0)?,
                        thread_id: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(store_err)
    }

    /// Locate a row by the server locator first. This matters after a local
    /// move has already rehomed a message: its stable logical id no longer
    /// has the conventional `msg:{account}:{folder}:{uid}` spelling.
    fn by_locator(&self, uid: u32) -> Result<Option<ExistingMessage>, SyncError> {
        self.db
            .conn()
            .query_row(
                "SELECT id, thread_id, folder_id, provider_uid
                 FROM messages
                 WHERE account_id = ?1 AND folder_id = ?2 AND provider_uid = ?3
                 LIMIT 1",
                params![self.account_id, self.folder_id, i64::from(uid)],
                |row| {
                    Ok(ExistingMessage {
                        id: row.get(0)?,
                        thread_id: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(store_err)
    }

    /// Destination-side lookup for an active/ACKed own move. A destination
    /// UID is authoritative; before it is known, a unique Message-ID or a
    /// unique `(date,size)` pair is the safe discriminator. Ambiguous
    /// metadata deliberately returns `None` so an unrelated message can
    /// never steal another message's stable id.
    fn destination_intent(
        &self,
        h: &HeaderMeta,
        date: Option<i64>,
    ) -> Result<Option<MoveIntent>, SyncError> {
        let conn = self.db.conn();
        let mut stmt = conn
            .prepare(
                "SELECT om.operation_id, om.message_id, om.destination_uid,
                        m.thread_id, m.message_id_header, m.date, m.size_bytes
                 FROM operation_moves om
                 JOIN operations o ON o.id = om.operation_id
                 JOIN messages m ON m.id = om.message_id
                 WHERE o.account_id = ?1 AND om.destination_folder_id = ?2
                   AND o.status IN ('pending', 'running', 'acked')",
            )
            .map_err(store_err)?;
        let rows = stmt
            .query_map(params![self.account_id, self.folder_id], |row| {
                Ok(MoveCandidate {
                    operation_id: row.get(0)?,
                    message_id: row.get(1)?,
                    destination_uid: row.get(2)?,
                    thread_id: row.get(3)?,
                    message_id_header: row.get(4)?,
                    date: row.get(5)?,
                    size_bytes: row.get(6)?,
                })
            })
            .map_err(store_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(store_err)?;
        let message_id = h.message_id.as_deref().filter(|v| !v.trim().is_empty());
        let size = h.size_bytes.map(|v| v as i64);
        let mut matches = rows.into_iter().filter(|candidate| {
            if candidate.destination_uid == Some(i64::from(h.uid)) {
                return true;
            }
            if candidate.destination_uid.is_some() {
                return false;
            }
            if let (Some(incoming), Some(stored)) =
                (message_id, candidate.message_id_header.as_deref())
            {
                return incoming == stored;
            }
            // Do not use date/size as a fallback when a Message-ID exists on
            // only one side: that would let a malformed header collide with
            // a different message that happens to share its size.
            message_id.is_none()
                && candidate.message_id_header.is_none()
                && date.is_some()
                && size.is_some()
                && Some(candidate.date) == date
                && Some(candidate.size_bytes) == size
        });
        let Some(candidate) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Ok(None);
        }
        Ok(Some(MoveIntent {
            operation_id: candidate.operation_id,
            message_id: candidate.message_id,
            thread_id: candidate.thread_id,
        }))
    }

    /// Source-side lookup keeps a source header from creating a duplicate
    /// after destination sync happened first and rehomed the stable row.
    fn source_intent(&self, uid: u32) -> Result<Option<MoveIntent>, SyncError> {
        self.db
            .conn()
            .query_row(
                "SELECT om.operation_id, om.message_id, om.destination_folder_id,
                        om.destination_uid, m.thread_id, m.folder_id,
                        m.provider_uid
                 FROM operation_moves om
                 JOIN operations o ON o.id = om.operation_id
                 JOIN messages m ON m.id = om.message_id
                 WHERE o.account_id = ?1 AND om.source_folder_id = ?2
                   AND om.source_uid = ?3
                   AND o.status IN ('pending', 'running', 'acked')
                 ORDER BY o.created_at DESC, om.operation_id DESC
                 LIMIT 1",
                params![self.account_id, self.folder_id, i64::from(uid)],
                |row| {
                    Ok(MoveIntent {
                        operation_id: row.get(0)?,
                        message_id: row.get(1)?,
                        thread_id: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(store_err)
    }

    /// Remove a stale destination representation before the stable source
    /// row is rehomed.  This is intentionally scoped to a matched own move:
    /// a normal folder sync must never delete a row merely because two
    /// messages happen to share a Message-ID.  The provider UID or the
    /// matched Message-ID identifies the duplicate representation of this
    /// exact remote message; operation references protect a row that another
    /// in-flight move still needs.
    fn remove_destination_duplicates(
        &self,
        h: &HeaderMeta,
        intent: &MoveIntent,
    ) -> Result<(), SyncError> {
        let conn = self.db.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, thread_id FROM messages
                 WHERE account_id = ?1 AND folder_id = ?2 AND id <> ?3
                   AND (
                       provider_uid = ?4
                       OR (?5 IS NOT NULL AND ?5 <> '' AND message_id_header = ?5)
                   )",
            )
            .map_err(store_err)?;
        let duplicates = stmt
            .query_map(
                params![
                    self.account_id,
                    self.folder_id,
                    intent.message_id,
                    i64::from(h.uid),
                    h.message_id.as_deref(),
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(store_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(store_err)?;
        for (message_id, thread_id) in duplicates {
            let referenced: bool = conn
                .query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM operation_moves WHERE message_id = ?1
                    )",
                    params![message_id],
                    |row| row.get(0),
                )
                .map_err(store_err)?;
            if referenced {
                continue;
            }
            conn.execute(
                "DELETE FROM messages_fts WHERE rowid IN \
                 (SELECT fts_rowid FROM fts_message_rows WHERE message_id = ?1)",
                params![message_id],
            )
            .map_err(store_err)?;
            conn.execute("DELETE FROM messages WHERE id = ?1", params![message_id])
                .map_err(store_err)?;
            conn.execute(
                "DELETE FROM threads WHERE id = ?1
                 AND NOT EXISTS (SELECT 1 FROM messages WHERE thread_id = ?1)",
                params![thread_id],
            )
            .map_err(store_err)?;
        }
        Ok(())
    }

    fn upsert_one(&self, h: &HeaderMeta) -> Result<String, SyncError> {
        let conn = self.db.conn();
        let msg_id = self.message_row_id(h.uid);
        let unread = !has_flag(&h.flags, "\\Seen");
        let starred = has_flag(&h.flags, "\\Flagged");
        let date = parsed_date(h);
        // IMAP deliberately hands the sync boundary the header values as
        // received. Decode RFC 2047 exactly once before persistence, so the
        // list, reading pane, and search index all share human-readable
        // display text instead of each GTK consumer attempting its own
        // partial decoder. Message-ID style fields below remain opaque.
        let decoded_from = h.from.as_deref().map(decode_encoded_words);
        let decoded_to = h.to.as_deref().map(decode_encoded_words);
        let decoded_cc = h.cc.as_deref().map(decode_encoded_words);
        let decoded_subject = h.subject.as_deref().map(decode_encoded_words);
        let (sender_name, sender_email) = match decoded_from.as_deref() {
            Some(raw) => {
                let (name, email) = split_display_address(raw);
                (Some(name), Some(email))
            }
            None => (None, None),
        };
        let references = if h.references.is_empty() {
            None
        } else {
            Some(h.references.join(" "))
        };
        let size_bytes = h.size_bytes.map(|v| v as i64);

        let destination_intent = self.destination_intent(h, date)?;
        let source_intent = if destination_intent.is_none() {
            self.source_intent(h.uid)?
        } else {
            None
        };
        if let Some(intent) = destination_intent.as_ref() {
            self.remove_destination_duplicates(h, intent)?;
        }
        let existing = destination_intent
            .as_ref()
            .or(source_intent.as_ref())
            .map(|intent| ExistingMessage {
                id: intent.message_id.clone(),
                thread_id: intent.thread_id.clone(),
            })
            .or(self.by_locator(h.uid)?)
            .or(self.existing_message(&msg_id)?);

        if let Some(existing) = existing {
            let rehome = destination_intent.as_ref();
            conn.execute(
                "UPDATE messages SET
                    message_id_header = COALESCE(?1, message_id_header),
                    in_reply_to = COALESCE(?2, in_reply_to),
                    references_header = COALESCE(?3, references_header),
                    date = COALESCE(?4, date),
                    sender_name = COALESCE(?5, sender_name),
                    sender_email = COALESCE(?6, sender_email),
                    recipients = COALESCE(?7, recipients),
                    cc = COALESCE(?8, cc),
                    subject = COALESCE(?9, subject),
                    size_bytes = COALESCE(?10, size_bytes),
                    unread = ?11,
                    starred = ?12,
                    folder_id = COALESCE(?13, folder_id),
                    provider_uid = COALESCE(?14, provider_uid)
                 WHERE id = ?15",
                params![
                    h.message_id,
                    h.in_reply_to,
                    references,
                    date,
                    sender_name,
                    sender_email,
                    decoded_to,
                    decoded_cc,
                    decoded_subject,
                    size_bytes,
                    unread,
                    starred,
                    rehome.map(|_| self.folder_id.as_str()),
                    rehome.map(|_| i64::from(h.uid)),
                    existing.id,
                ],
            )
            .map_err(store_err)?;
            if let Some(intent) = rehome {
                conn.execute(
                    "UPDATE threads SET folder_id = ?1, archived = 0, deleted = 0,
                        snooze_until = NULL WHERE id = ?2 AND account_id = ?3",
                    params![self.folder_id, existing.thread_id, self.account_id],
                )
                .map_err(store_err)?;
                conn.execute(
                    "UPDATE operation_moves SET destination_uid = ?1
                     WHERE operation_id = ?2 AND message_id = ?3",
                    params![i64::from(h.uid), intent.operation_id, intent.message_id],
                )
                .map_err(store_err)?;
                // If Undo was clicked while the original move was already
                // sent, its reverse row was deliberately blocked because
                // the destination UID was unknown. Materialize the reverse
                // locator now that this destination header supplies it.
                conn.execute(
                    "INSERT OR IGNORE INTO operation_moves
                     (operation_id, message_id, source_folder_id, source_remote_id,
                      source_uid, destination_folder_id, destination_remote_id,
                      destination_uid)
                     SELECT reverse.id, om.message_id,
                            om.destination_folder_id, om.destination_remote_id,
                            om.destination_uid, om.source_folder_id,
                            om.source_remote_id, NULL
                     FROM operation_moves om
                     JOIN operations reverse ON reverse.undo_of = om.operation_id
                     WHERE om.operation_id = ?1 AND om.message_id = ?2
                       AND reverse.status = 'blocked'
                       AND om.destination_uid IS NOT NULL",
                    params![intent.operation_id, intent.message_id],
                )
                .map_err(store_err)?;
                // Source-first order can observe the ACKed source UID
                // disappearing before destination sync learns its new UID.
                // In that case `remove_vanished` already rehomed this stable
                // row, so this destination observation is the second and
                // final reconciliation fact.  Do not leave a forever-live
                // intent behind waiting for a source UID that is already
                // gone; destination-first remains protected because its
                // source row is still present here.
                conn.execute(
                    "INSERT OR IGNORE INTO operation_move_history
                     (operation_id, message_id, source_folder_id, source_remote_id,
                      source_uid, destination_folder_id, destination_remote_id,
                      destination_uid, recorded_at)
                     SELECT operation_id, message_id, source_folder_id, source_remote_id,
                            source_uid, destination_folder_id, destination_remote_id,
                            destination_uid, strftime('%s','now')
                     FROM operation_moves WHERE operation_id = ?1 AND message_id = ?2",
                    params![intent.operation_id, intent.message_id],
                )
                .map_err(store_err)?;
                conn.execute(
                    "DELETE FROM operation_moves
                     WHERE operation_id = ?1 AND message_id = ?2
                       AND EXISTS (
                           SELECT 1 FROM operations
                           WHERE id = ?1 AND status = 'acked'
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM messages m
                           WHERE m.id = operation_moves.message_id
                             AND m.folder_id = operation_moves.source_folder_id
                             AND m.provider_uid = operation_moves.source_uid
                       )",
                    params![intent.operation_id, intent.message_id],
                )
                .map_err(store_err)?;
                // Destination sync may be the first observation that makes
                // a running Undo reverse operation actionable. Release it
                // only after the active intent was resolved/archived.
                conn.execute(
                    "UPDATE operations SET status = 'pending', next_attempt_at = NULL
                     WHERE undo_of = ?1 AND status = 'blocked'
                       AND NOT EXISTS (
                           SELECT 1 FROM operation_moves om
                           WHERE om.operation_id = ?1 AND om.destination_uid IS NULL
                       )
                       AND NOT EXISTS (
                           SELECT 1 FROM operation_move_history h
                           WHERE h.operation_id = ?1 AND h.destination_uid IS NULL
                       )",
                    params![intent.operation_id],
                )
                .map_err(store_err)?;
            }
            conn.execute(
                "UPDATE threads SET gm_thrid = COALESCE(?1, gm_thrid)
                 WHERE id = (SELECT thread_id FROM messages WHERE id = ?2)",
                params![h.gm_thrid, existing.id],
            )
            .map_err(store_err)?;
            Ok(existing.thread_id)
        } else {
            let thread_id = self.thread_row_id(h.uid);
            // Capture the "nothing parsed at all" signal before `date` gets
            // shadowed by its zero-fallback below.
            let date_is_unknown = date.is_none();
            let date = date.unwrap_or(0);
            let (sender_name, sender_email) = match (sender_name, sender_email) {
                (Some(n), Some(e)) => (n, e),
                _ => (String::new(), String::new()),
            };
            // A message this bare -- no Message-ID, no From, no Subject, and
            // neither the RFC822 `Date:` header nor the server's own
            // INTERNALDATE parsed -- is not "a terse email", it is what a
            // garbled/truncated RFC822 header block degrades to once the
            // wire parser gives up on it (T-027). Note this is a brand-new
            // row only: an existing message never re-enters this branch, so
            // a legitimate flags-only CONDSTORE update (which *also* leaves
            // every field but uid/flags at its default, see
            // `HeaderMeta`'s doc comment) can never be mistaken for this --
            // it always hits the `exists` branch above instead, where a
            // `None` just leaves the previously-stored subject alone via
            // `COALESCE`.
            let is_unparseable = h.message_id.is_none()
                && sender_name.is_empty()
                && sender_email.is_empty()
                && decoded_subject
                    .as_deref()
                    .is_none_or(|s| s.trim().is_empty())
                && date_is_unknown;
            let subject = if is_unparseable {
                UNPARSEABLE_SUBJECT.to_string()
            } else {
                decoded_subject.unwrap_or_default()
            };
            let recipients = decoded_to.unwrap_or_default();

            conn.execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred, gm_thrid)
                 VALUES (?1, ?2, ?3, ?4, '', ?5, ?6, ?7, ?8)",
                params![
                    thread_id,
                    self.account_id,
                    self.folder_id,
                    subject,
                    date,
                    unread,
                    starred,
                    h.gm_thrid,
                ],
            )
            .map_err(store_err)?;

            conn.execute(
                "INSERT INTO messages (
                    id, account_id, thread_id, folder_id, provider_uid,
                    message_id_header, in_reply_to, references_header, date,
                    sender_name, sender_email, recipients, cc, subject, snippet,
                    unread, starred, has_attachment, importance, size_bytes
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5,
                    ?6, ?7, ?8, ?9,
                    ?10, ?11, ?12, ?13, ?14, '',
                    ?15, ?16, 0, 0, ?17
                 )",
                params![
                    msg_id,
                    self.account_id,
                    thread_id,
                    self.folder_id,
                    h.uid,
                    h.message_id,
                    h.in_reply_to,
                    references,
                    date,
                    sender_name,
                    sender_email,
                    recipients,
                    decoded_cc.unwrap_or_default(),
                    subject,
                    unread,
                    starred,
                    size_bytes.unwrap_or(0),
                ],
            )
            .map_err(store_err)?;

            // T-048: a brand-new message row is not yet searchable --
            // queue it for the background indexer instead of indexing it
            // inline here, so header sync never waits on FTS work (and
            // the body, which this branch never has, gets picked up later
            // via `Core::store_body`'s own enqueue -- see
            // `crate::search`'s module doc). Deliberately only on this
            // "brand new row" branch, not the `exists` branch above: a
            // flags-only CONDSTORE update leaves subject/sender/etc.
            // untouched (`COALESCE(?, existing)`), so there is nothing
            // stale in `messages_fts` for that case to fix.
            crate::search::enqueue_for_indexing(conn, &msg_id).map_err(store_err)?;
            Ok(thread_id)
        }
    }

    /// Load this folder's headers, assign surviving thread ids, retarget
    /// queue/snooze/draft rows off absorbed ids, drop empty threads, and
    /// rewrite rollup columns. Safe to run inside an already-open
    /// transaction (it does not start its own).
    ///
    /// `seed_thread_ids` is `None` only on the one-shot `rethread_folder`
    /// path (whole-folder rollup after copying flags down). The hot
    /// `upsert_headers` path passes the batch's thread ids so a neighbour's
    /// optimistic unread/starred is not rewritten.
    fn assign_and_rollup(&self, seed_thread_ids: Option<&[String]>) -> Result<(), SyncError> {
        let conn = self.db.conn();
        let hints = self.load_hints()?;
        if hints.is_empty() {
            return Ok(());
        }
        let assigned = assign_groups(&hints);
        let mut retarget: HashMap<String, String> = HashMap::new();
        for (h, dest) in hints.iter().zip(assigned.iter()) {
            if h.row_id != *dest {
                retarget.insert(h.row_id.clone(), dest.clone());
            }
        }
        for (absorbed, survivor) in &retarget {
            conn.execute(
                "UPDATE threads SET gm_thrid = COALESCE(gm_thrid, (SELECT gm_thrid FROM threads WHERE id = ?1))
                 WHERE id = ?2",
                params![absorbed, survivor],
            )
            .map_err(store_err)?;
            conn.execute(
                "UPDATE messages SET thread_id = ?1 WHERE thread_id = ?2",
                params![survivor, absorbed],
            )
            .map_err(store_err)?;
            self.retarget_refs(survivor, absorbed)?;
        }
        match seed_thread_ids {
            None => self.rollup_folder(None)?,
            Some(seed) => {
                let mut affected: HashSet<String> = HashSet::new();
                for id in seed {
                    affected.insert(retarget.get(id).cloned().unwrap_or_else(|| id.clone()));
                }
                for survivor in retarget.values() {
                    affected.insert(survivor.clone());
                }
                let affected: Vec<String> = affected.into_iter().collect();
                self.rollup_folder(Some(&affected))?;
            }
        }
        for absorbed in retarget.keys() {
            conn.execute(
                "DELETE FROM threads WHERE id = ?1
                 AND NOT EXISTS (SELECT 1 FROM messages WHERE messages.thread_id = threads.id)",
                params![absorbed],
            )
            .map_err(store_err)?;
        }
        Ok(())
    }

    fn load_hints(&self) -> Result<Vec<ThreadHint>, SyncError> {
        let conn = self.db.conn();
        let mut stmt = conn
            .prepare(
                "SELECT m.thread_id, m.message_id_header, m.in_reply_to, m.references_header, t.gm_thrid
                 FROM messages m
                 JOIN threads t ON t.id = m.thread_id
                 WHERE m.account_id = ?1 AND m.folder_id = ?2
                 ORDER BY m.id",
            )
            .map_err(store_err)?;
        let rows = stmt
            .query_map(params![self.account_id, self.folder_id], |r| {
                let refs: Option<String> = r.get(3)?;
                let gm: Option<String> = r.get(4)?;
                Ok(ThreadHint {
                    row_id: r.get(0)?,
                    message_id: r.get(1)?,
                    in_reply_to: r.get(2)?,
                    references: refs
                        .unwrap_or_default()
                        .split_whitespace()
                        .map(str::to_string)
                        .collect(),
                    gm_thrid: gm.filter(|s| !s.is_empty()),
                })
            })
            .map_err(store_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(store_err)?;
        Ok(rows)
    }

    fn retarget_refs(&self, survivor: &str, absorbed: &str) -> Result<(), SyncError> {
        let conn = self.db.conn();
        let survivor_snoozed: bool = conn
            .query_row(
                "SELECT 1 FROM snoozes WHERE account_id = ?1 AND thread_id = ?2",
                params![self.account_id, survivor],
                |_| Ok(()),
            )
            .optional()
            .map_err(store_err)?
            .is_some();
        if survivor_snoozed {
            conn.execute(
                "DELETE FROM snoozes WHERE thread_id = ?1",
                params![absorbed],
            )
            .map_err(store_err)?;
        } else {
            conn.execute(
                "UPDATE snoozes SET thread_id = ?1 WHERE thread_id = ?2",
                params![survivor, absorbed],
            )
            .map_err(store_err)?;
        }
        conn.execute(
            "UPDATE drafts SET thread_id = ?1 WHERE thread_id = ?2",
            params![survivor, absorbed],
        )
        .map_err(store_err)?;

        let pending: Vec<(String, String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT id, op, payload_hash FROM operations
                     WHERE target_id = ?1 AND status IN ('pending', 'running')",
                )
                .map_err(store_err)?;
            let out = stmt
                .query_map(params![absorbed], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(store_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(store_err)?;
            out
        };
        for (id, op, hash) in pending {
            let collide: bool = conn
                .query_row(
                    "SELECT 1 FROM operations
                     WHERE account_id = ?1 AND op = ?2 AND target_id = ?3
                       AND payload_hash = ?4 AND status IN ('pending', 'running')",
                    params![self.account_id, op, survivor, hash],
                    |_| Ok(()),
                )
                .optional()
                .map_err(store_err)?
                .is_some();
            if collide {
                conn.execute("DELETE FROM operations WHERE id = ?1", params![id])
                    .map_err(store_err)?;
            } else {
                conn.execute(
                    "UPDATE operations SET target_id = ?1 WHERE id = ?2",
                    params![survivor, id],
                )
                .map_err(store_err)?;
            }
        }
        conn.execute(
            "UPDATE operations SET target_id = ?1 WHERE target_id = ?2",
            params![survivor, absorbed],
        )
        .map_err(store_err)?;
        Ok(())
    }

    /// Rewrite rollup columns. `thread_ids = None` is the one-shot
    /// `rethread_folder` path (whole folder). `Some` is the hot path:
    /// only those ids, never `WHERE folder_id` without an id filter —
    /// that is what would clobber an optimistic MarkRead on a neighbour.
    fn rollup_folder(&self, thread_ids: Option<&[String]>) -> Result<(), SyncError> {
        const SET: &str = "UPDATE threads SET
                    message_count = (SELECT COUNT(*) FROM messages m WHERE m.thread_id = threads.id),
                    date = COALESCE(
                        (SELECT m.date FROM messages m WHERE m.thread_id = threads.id
                         ORDER BY m.date DESC, m.id DESC LIMIT 1),
                        date),
                    subject = COALESCE(
                        (SELECT m.subject FROM messages m WHERE m.thread_id = threads.id
                         ORDER BY m.date DESC, m.id DESC LIMIT 1),
                        subject),
                    snippet = COALESCE(
                        (SELECT m.snippet FROM messages m WHERE m.thread_id = threads.id
                         ORDER BY m.date DESC, m.id DESC LIMIT 1),
                        snippet),
                    unread = COALESCE(
                        (SELECT MAX(m.unread) FROM messages m WHERE m.thread_id = threads.id),
                        0),
                    starred = COALESCE(
                        (SELECT MAX(m.starred) FROM messages m WHERE m.thread_id = threads.id),
                        0),
                    has_attachment = COALESCE(
                        (SELECT MAX(m.has_attachment) FROM messages m WHERE m.thread_id = threads.id),
                        0)";
        let conn = self.db.conn();
        match thread_ids {
            None => {
                conn.execute(
                    &format!(
                        "{SET}
                 WHERE account_id = ?1 AND folder_id = ?2
                   AND EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = threads.id)"
                    ),
                    params![self.account_id, self.folder_id],
                )
                .map_err(store_err)?;
            }
            Some([]) => return Ok(()),
            Some(ids) => {
                let placeholders = vec!["?"; ids.len()].join(",");
                let sql = format!(
                    "{SET}
                 WHERE account_id = ? AND folder_id = ? AND id IN ({placeholders})
                   AND EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = threads.id)"
                );
                let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&self.account_id, &self.folder_id];
                for id in ids {
                    bind.push(id);
                }
                conn.execute(&sql, bind.as_slice()).map_err(store_err)?;
            }
        }
        Ok(())
    }

    /// Group this folder's already-stored messages. Public so [`crate::Core::open`]
    /// can one-shot live 1:1 profiles (flag in settings, not a DDL bump).
    pub fn rethread_folder(&self) -> Result<(), SyncError> {
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(store_err)?;
        // Live profiles already marked threads read via T-028 dispatch, but
        // `messages.unread` still holds the IMAP value. Copy down before
        // grouping so whole-folder rollup cannot resurrect unread.
        conn.execute(
            "UPDATE messages SET
                unread = (SELECT t.unread FROM threads t WHERE t.id = messages.thread_id),
                starred = (SELECT t.starred FROM threads t WHERE t.id = messages.thread_id)
             WHERE account_id = ?1 AND folder_id = ?2",
            params![self.account_id, self.folder_id],
        )
        .map_err(store_err)?;
        self.assign_and_rollup(None)?;
        tx.commit().map_err(store_err)?;
        Ok(())
    }
}

impl SyncStore for CoreSyncStore<'_> {
    fn load_state(&mut self, _folder: &str) -> Result<FolderSyncState, SyncError> {
        let row = self
            .db
            .conn()
            .query_row(
                "SELECT uidvalidity, uidnext, highest_modseq, last_sync_at, backfill_floor, backfill_target
                 FROM sync_state WHERE account_id = ?1 AND folder_id = ?2",
                params![self.account_id, self.folder_id],
                |r| {
                    Ok(FolderSyncState {
                        uidvalidity: r.get::<_, Option<i64>>(0)?.map(|v| v as u32),
                        uidnext: r.get::<_, Option<i64>>(1)?.map(|v| v as u32),
                        highest_modseq: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                        last_synced_at: r.get(3)?,
                        backfill_floor: r.get::<_, Option<i64>>(4)?.map(|v| v as u32),
                        backfill_target: r.get::<_, Option<i64>>(5)?.map(|v| v as u32),
                    })
                },
            )
            .optional()
            .map_err(store_err)?;
        Ok(row.unwrap_or_default())
    }

    fn save_state(&mut self, _folder: &str, state: &FolderSyncState) -> Result<(), SyncError> {
        self.db
            .conn()
            .execute(
                "INSERT INTO sync_state
                    (account_id, folder_id, uidvalidity, uidnext, highest_modseq, last_sync_at, backfill_floor, backfill_target)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(account_id, folder_id) DO UPDATE SET
                    uidvalidity = excluded.uidvalidity,
                    uidnext = excluded.uidnext,
                    highest_modseq = excluded.highest_modseq,
                    last_sync_at = excluded.last_sync_at,
                    backfill_floor = excluded.backfill_floor,
                    backfill_target = excluded.backfill_target",
                params![
                    self.account_id,
                    self.folder_id,
                    state.uidvalidity.map(i64::from),
                    state.uidnext.map(i64::from),
                    state.highest_modseq.map(|v| v as i64),
                    state.last_synced_at,
                    state.backfill_floor.map(i64::from),
                    state.backfill_target.map(i64::from),
                ],
            )
            .map_err(store_err)?;
        Ok(())
    }

    fn upsert_headers(&mut self, _folder: &str, headers: &[HeaderMeta]) -> Result<(), SyncError> {
        // Empty CONDSTORE still calls this with `&[]`. Grouping + whole-
        // folder rollup would rewrite every thread's unread/starred from
        // `messages` and undo a neighbour's optimistic MarkRead. No work.
        if headers.is_empty() {
            return Ok(());
        }
        // One transaction per batch (D13 "batched writes"). The engine only
        // advances `backfill_floor` after a batch returns Ok, so a crash
        // mid-batch is already safe by re-run -- `upsert_one` is idempotent
        // on the same uid. The transaction is here for the *error* path
        // instead: a failure on header 150 of 200 otherwise leaves 149
        // messages committed under a floor that never moved, and the retry
        // re-walks them. Rolling back keeps "batch succeeded" and "batch is
        // visible" the same statement. Measured cost of the grouping on a
        // 200-header batch: 19.5ms autocommit vs 16.2ms in one transaction.
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(store_err)?;
        let mut seed = Vec::with_capacity(headers.len());
        for h in headers {
            seed.push(self.upsert_one(h)?);
        }
        self.assign_and_rollup(Some(&seed))?;
        tx.commit().map_err(store_err)?;
        Ok(())
    }

    fn remove_vanished(&mut self, _folder: &str, uids: &[u32]) -> Result<(), SyncError> {
        if uids.is_empty() {
            return Ok(());
        }
        let conn = self.db.conn();
        let placeholders = vec!["?"; uids.len()].join(",");
        // A source UID that vanished after our own operation was ACKed is
        // not an external deletion. Rehome its stable row and retain the
        // intent until destination sync has supplied a UID (or clear it now
        // when that UID is already known). Pending/running/failed intents do
        // not get this exception: normal deletion semantics stay unchanged.
        let acked_moves: Vec<(String, String, String, Option<i64>, String)> = {
            let sql = format!(
                "SELECT om.operation_id, om.message_id, om.destination_folder_id,
                        om.destination_uid, om.source_folder_id
                 FROM operation_moves om
                 JOIN operations o ON o.id = om.operation_id
                 WHERE o.account_id = ? AND o.status = 'acked'
                   AND om.source_folder_id = ?
                   AND om.source_uid IN ({placeholders})"
            );
            let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&self.account_id, &self.folder_id];
            for u in uids {
                bind.push(u);
            }
            let mut stmt = conn.prepare(&sql).map_err(store_err)?;
            let rows = stmt
                .query_map(bind.as_slice(), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .map_err(store_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(store_err)?;
            rows
        };
        let ids: Vec<(String, String)> = {
            let sql = format!(
                "SELECT id, thread_id FROM messages \
                 WHERE account_id = ? AND folder_id = ? AND provider_uid IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql).map_err(store_err)?;
            let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&self.account_id, &self.folder_id];
            for u in uids {
                bind.push(u);
            }
            let rows = stmt
                .query_map(bind.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(store_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(store_err)?;
            rows
        };
        if ids.is_empty() && acked_moves.is_empty() {
            return Ok(());
        }
        let preserved: std::collections::HashSet<&str> = acked_moves
            .iter()
            .map(|(_, id, _, _, _)| id.as_str())
            .collect();
        let ids: Vec<(String, String)> = ids
            .into_iter()
            .filter(|(id, _)| !preserved.contains(id.as_str()))
            .collect();
        let msg_ids: Vec<String> = ids.iter().map(|(id, _)| id.clone()).collect();
        let thread_ids: Vec<String> = ids.iter().map(|(_, t)| t.clone()).collect();

        // The three deletes below are one unit. Ordering already protects
        // the direction that would leak (message row gone, its indexed body
        // left behind in `messages_fts`), but the opposite split is a
        // silently unsearchable message: the fts row is dropped, the
        // `DELETE FROM messages` then fails, and nothing re-indexes it
        // because the message still looks synced. Commit or roll back both.
        let tx = conn.unchecked_transaction().map_err(store_err)?;

        for (operation_id, message_id, destination_folder_id, destination_uid, source_folder_id) in
            &acked_moves
        {
            tx.execute(
                "UPDATE messages SET folder_id = ?1,
                    provider_uid = CASE WHEN ?2 IS NULL THEN NULL ELSE ?2 END
                 WHERE account_id = ?3 AND id = ?4",
                params![
                    destination_folder_id,
                    destination_uid,
                    self.account_id,
                    message_id
                ],
            )
            .map_err(store_err)?;
            tx.execute(
                "UPDATE threads SET folder_id = ?1, archived = 0, deleted = 0,
                    snooze_until = NULL
                 WHERE account_id = ?2 AND id = (SELECT thread_id FROM messages WHERE id = ?3)
                   AND folder_id = ?4",
                params![
                    destination_folder_id,
                    self.account_id,
                    message_id,
                    source_folder_id
                ],
            )
            .map_err(store_err)?;
            // A known destination UID means destination sync already saw
            // this message. Source sync has now also proved the old UID is
            // gone, so the durable intent has completed safely.
            if destination_uid.is_some() {
                tx.execute(
                    "INSERT OR IGNORE INTO operation_move_history
                     (operation_id, message_id, source_folder_id, source_remote_id,
                      source_uid, destination_folder_id, destination_remote_id,
                      destination_uid, recorded_at)
                     SELECT operation_id, message_id, source_folder_id, source_remote_id,
                            source_uid, destination_folder_id, destination_remote_id,
                            destination_uid, strftime('%s','now')
                     FROM operation_moves WHERE operation_id = ?1 AND message_id = ?2",
                    params![operation_id, message_id],
                )
                .map_err(store_err)?;
                tx.execute(
                    "DELETE FROM operation_moves WHERE operation_id = ?1 AND message_id = ?2",
                    params![operation_id, message_id],
                )
                .map_err(store_err)?;
            }
        }

        let msg_placeholders = vec!["?"; msg_ids.len()].join(",");
        // Delete the FTS rows for these messages first: `messages_fts` is a
        // plain FTS5 virtual table with no FK to `messages` (and no
        // `account_id` column of its own), so nothing cascades it. Its
        // `message_id` is deliberately UNINDEXED; v19's ordinary map lets
        // this bulk lifecycle deletion seek FTS rowids instead of scanning
        // the entire search index.
        if !msg_ids.is_empty() {
            conn.execute(
                &format!(
                    "DELETE FROM messages_fts WHERE rowid IN \
                     (SELECT fts_rowid FROM fts_message_rows \
                      WHERE message_id IN ({msg_placeholders}))"
                ),
                rusqlite::params_from_iter(msg_ids.iter()),
            )
            .map_err(store_err)?;
            conn.execute(
                &format!("DELETE FROM messages WHERE id IN ({msg_placeholders})"),
                rusqlite::params_from_iter(msg_ids.iter()),
            )
            .map_err(store_err)?;
        }

        // A vanished UID may leave a still-populated thread: drop only the
        // now-empty rows, then rewrite rollup on what remains.
        let thread_placeholders = vec!["?"; thread_ids.len()].join(",");
        if !thread_ids.is_empty() {
            conn.execute(
                &format!(
                    "DELETE FROM threads WHERE id IN ({thread_placeholders}) \
                     AND NOT EXISTS (SELECT 1 FROM messages WHERE messages.thread_id = threads.id)"
                ),
                rusqlite::params_from_iter(thread_ids.iter()),
            )
            .map_err(store_err)?;
            self.rollup_folder(Some(&thread_ids))?;
        }

        tx.commit().map_err(store_err)?;
        Ok(())
    }

    fn reset_folder(&mut self, _folder: &str) -> Result<(), SyncError> {
        // Reached when UIDVALIDITY changed, i.e. the whole folder is being
        // thrown away and re-pulled. Half a wipe is the worst outcome here.
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(store_err)?;
        conn.execute(
            "DELETE FROM messages_fts WHERE rowid IN \
             (SELECT fts_rowid FROM fts_message_rows WHERE message_id IN \
              (SELECT id FROM messages WHERE account_id = ?1 AND folder_id = ?2))",
            params![self.account_id, self.folder_id],
        )
        .map_err(store_err)?;
        conn.execute(
            "DELETE FROM messages WHERE account_id = ?1 AND folder_id = ?2",
            params![self.account_id, self.folder_id],
        )
        .map_err(store_err)?;
        conn.execute(
            "DELETE FROM threads WHERE account_id = ?1 AND folder_id = ?2",
            params![self.account_id, self.folder_id],
        )
        .map_err(store_err)?;
        tx.commit().map_err(store_err)?;
        Ok(())
    }
}

fn store_err(e: rusqlite::Error) -> SyncError {
    SyncError::Store(e.to_string())
}

fn has_flag(flags: &[String], name: &str) -> bool {
    flags.iter().any(|f| f.eq_ignore_ascii_case(name))
}

fn parsed_date(h: &HeaderMeta) -> Option<i64> {
    h.date
        .as_deref()
        .and_then(parse_rfc2822_date)
        .or_else(|| h.internaldate.as_deref().and_then(parse_internaldate))
}

/// Best-effort split of a raw `From`/`To` header value into a display name
/// and an email address. Handles the two common shapes (`Name <addr>` and a
/// bare `addr`); anything else is kept as a name with no email rather than
/// dropped. Full RFC 5322 mailbox-list parsing (groups, comments, multiple
/// addresses) is out of scope for T-022 -- bodies/attachments (T-024) and
/// full addressing already need a real parser and should share one then.
fn split_display_address(raw: &str) -> (String, String) {
    let raw = raw.trim();
    if let (Some(start), Some(end)) = (raw.find('<'), raw.rfind('>')) {
        if start < end {
            let name = raw[..start].trim().trim_matches('"').trim().to_string();
            let email = raw[start + 1..end].trim().to_string();
            return (name, email);
        }
    }
    if raw.contains('@') && !raw.contains(' ') {
        (String::new(), raw.to_string())
    } else {
        (raw.to_string(), String::new())
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

fn month_index(s: &str) -> Option<u32> {
    if s.len() < 3 {
        return None;
    }
    let key = s[..3].to_ascii_lowercase();
    MONTHS.iter().position(|m| *m == key).map(|i| i as u32 + 1)
}

/// Days since the Unix epoch for a (proleptic Gregorian) civil date. Howard
/// Hinnant's `days_from_civil`: <http://howardhinnant.github.io/date_algorithms.html>.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]: Mar=0 .. Feb=11
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

fn to_epoch(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32, offset_minutes: i64) -> i64 {
    let days = days_from_civil(y, mo as i64, d as i64);
    days * 86400 + h as i64 * 3600 + mi as i64 * 60 + s as i64 - offset_minutes * 60
}

/// Numeric (`+HHMM`/`-HHMM`) or a handful of common obsolete named zones
/// (RFC 2822 4.3) -- anything else (an unrecognized military letter, a
/// typo) is treated as UTC rather than failing the whole parse.
fn zone_offset_minutes(zone: &str) -> i64 {
    let bytes = zone.as_bytes();
    if zone.len() == 5
        && (bytes[0] == b'+' || bytes[0] == b'-')
        && zone[1..].bytes().all(|b| b.is_ascii_digit())
    {
        let hh: i64 = zone[1..3].parse().unwrap_or(0);
        let mm: i64 = zone[3..5].parse().unwrap_or(0);
        let total = hh * 60 + mm;
        return if bytes[0] == b'-' { -total } else { total };
    }
    match zone.to_ascii_uppercase().as_str() {
        "UT" | "GMT" | "Z" => 0,
        "EST" => -5 * 60,
        "EDT" => -4 * 60,
        "CST" => -6 * 60,
        "CDT" => -5 * 60,
        "MST" => -7 * 60,
        "MDT" => -6 * 60,
        "PST" => -8 * 60,
        "PDT" => -7 * 60,
        _ => 0,
    }
}

/// A `Date:` header, e.g. `"Tue, 15 Nov 1994 08:12:31 -0500"`. The leading
/// weekday (with its comma) is optional and ignored either way.
fn parse_rfc2822_date(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let s = match s.find(',') {
        Some(idx) => s[idx + 1..].trim(),
        None => s,
    };
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.len() < 4 {
        return None;
    }
    let day: u32 = tokens[0].parse().ok()?;
    let month = month_index(tokens[1])?;
    let mut year: i64 = tokens[2].parse().ok()?;
    if year < 100 {
        // RFC 2822 obsolete 2-digit year (4.3): 0-49 -> 2000s, 50-99 -> 1900s.
        year += if year < 50 { 2000 } else { 1900 };
    }
    let mut time_parts = tokens[3].split(':');
    let hour: u32 = time_parts.next()?.parse().ok()?;
    let minute: u32 = time_parts.next()?.parse().ok()?;
    let second: u32 = time_parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    // A corrupted or hostile `Date:` header can carry an arbitrarily long
    // numeral in the year slot (e.g. "01 Jan 999999999999 00:00:00 +0000").
    // `year` still parses fine as an `i64`, but `to_epoch`'s
    // `days * 86400` overflows `i64` for anything but a sane calendar
    // year, which panics in a debug/test build and produces silent garbage
    // in release. Reject out-of-range years here instead of letting the
    // arithmetic below run on them -- one malformed message must not be
    // able to crash a whole sync batch (T-027).
    if !(1..=9999).contains(&year)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let offset_minutes = tokens.get(4).map(|z| zone_offset_minutes(z)).unwrap_or(0);
    Some(to_epoch(
        year,
        month,
        day,
        hour,
        minute,
        second,
        offset_minutes,
    ))
}

/// IMAP `INTERNALDATE`, e.g. `"01-Jan-2024 00:00:00 +0000"`.
fn parse_internaldate(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let (date_part, rest) = s.split_once(' ')?;
    let mut dparts = date_part.splitn(3, '-');
    let day: u32 = dparts.next()?.parse().ok()?;
    let month = month_index(dparts.next()?)?;
    let year: i64 = dparts.next()?.parse().ok()?;
    let mut rparts = rest.split_whitespace();
    let time = rparts.next()?;
    let zone = rparts.next().unwrap_or("+0000");
    let mut tparts = time.split(':');
    let hour: u32 = tparts.next()?.parse().ok()?;
    let minute: u32 = tparts.next()?.parse().ok()?;
    let second: u32 = tparts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    // Same overflow guard as `parse_rfc2822_date`: INTERNALDATE is
    // server-controlled, but treat it as untrusted input too rather than
    // assume the server can never send a mangled year.
    if !(1..=9999).contains(&year)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let offset = zone_offset_minutes(zone);
    Some(to_epoch(year, month, day, hour, minute, second, offset))
}

/// [`FolderKind`] -> [`FolderRole`] for [`Core::folder_sync_inputs`] (T-078
/// prep, part (b)). `feathermail_sync` cannot know this crate's folder kind
/// (see the module doc comment on `FolderRole` in
/// crates/sync/src/schedule.rs: the whole point of that local enum is that
/// `feathermail-sync` has no dependency on `feathermail-core`), so the
/// mapping has to live on this side of the boundary.
///
/// `Starred`/`Snoozed` are never real `folders` rows -- [`Core::list_folders`]'s
/// doc comment spells out why: on the server these are a flag and a
/// client-local idea, not a mailbox, so `Core::sync_folders` never
/// discovers one to insert. `folder_sync_inputs` reads straight off the
/// `folders` table, so this arm is never actually exercised today. Kept
/// explicit anyway (rather than a wildcard) so the match stays exhaustive:
/// if that overlay design ever changes and one of these kinds does get a
/// real row, it degrades to `Other` -- background-priority, same as any
/// other custom folder -- instead of silently failing to compile or
/// picking an arbitrary role.
fn folder_role(kind: FolderKind) -> FolderRole {
    match kind {
        FolderKind::Inbox => FolderRole::Inbox,
        FolderKind::Sent => FolderRole::Sent,
        FolderKind::Drafts => FolderRole::Drafts,
        FolderKind::Archive => FolderRole::Archive,
        FolderKind::Spam => FolderRole::Spam,
        FolderKind::Trash => FolderRole::Trash,
        FolderKind::Custom | FolderKind::Starred | FolderKind::Snoozed => FolderRole::Other,
    }
}

/// T-132: the whole outstanding first-run download as one fraction --
/// what a progress bar needs and nothing more. `done`/`total` are UIDs
/// walked, not messages stored: a UID window is what the backfill is
/// actually working through, and it is the one number that cannot go
/// backwards when a folder turns out to hold fewer messages than UIDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncProgress {
    pub done: u64,
    pub total: u64,
}

impl SyncProgress {
    /// `0.0..=1.0`, and never `NaN`: an empty total reads as finished,
    /// because a bar that shows nothing is better than one that shows
    /// `0/0`.
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 1.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
    }

    /// Whole per cent, rounded down so a bar that says 100 % is really
    /// finished.
    pub fn percent(&self) -> u32 {
        (self.fraction() * 100.0).floor() as u32
    }
}

/// T-132: fold `(backfill_target, backfill_floor)` windows into one
/// fraction. Pure, so the arithmetic is testable without a database.
///
/// Per folder: the backfill started at `target` and walks down to 1, so
/// `total = target - 1` and `done = target - floor`. A window that is
/// already at (or below) the bottom contributes nothing -- the engine
/// spells "finished" both as `NULL` and as `Some(1)`, and both mean the
/// same thing here.
fn sync_progress_from_windows(windows: &[(i64, i64)]) -> Option<SyncProgress> {
    let mut done: u64 = 0;
    let mut total: u64 = 0;
    let mut pending = false;
    for (target, floor) in windows {
        let (target, floor) = (*target, *floor);
        if target <= 1 || floor <= 1 {
            continue;
        }
        pending = true;
        total = total.saturating_add((target - 1).max(0) as u64);
        done = done.saturating_add((target - floor).max(0) as u64);
    }
    pending.then_some(SyncProgress { done, total })
}

impl Core {
    /// T-078 (b) seam: the only way for `crates/service` -- the
    /// composition root that sits *over* both `core` and `providers` (see
    /// plan.md T-078's layering note) -- to build a [`CoreSyncStore`] to
    /// hand to `feathermail_sync::sync_folder`. `CoreSyncStore::new` takes
    /// `&Database`, and [`Core::db`](crate::store::Core) is `pub(crate)`
    /// (D9: nothing outside this crate is meant to hold a raw connection),
    /// so before this method existed, `service` had no physical way to
    /// construct one. `folder` is the same `folders.id` string
    /// [`Core::folder_sync_inputs`] hands back in each `FolderInput::id` --
    /// not the IMAP mailbox name, see [`CoreSyncStore`]'s own doc comment.
    pub fn sync_store(&self, account: &AccountId, folder: &str) -> CoreSyncStore<'_> {
        CoreSyncStore::new(&self.db, account.as_str(), folder)
    }

    /// T-078 (b) prep: assembles [`feathermail_sync::schedule::next_sync`]'s
    /// per-folder input for every folder of one account, from `folders`
    /// LEFT JOIN `sync_state`.
    ///
    /// The join is a `LEFT JOIN`, not an inner one, deliberately: a folder
    /// that has never gone through a sync pass has no `sync_state` row at
    /// all yet, and "never synced" is a real, legitimate state the
    /// scheduler is required to see -- not a reason to drop the folder from
    /// the list. See [`FolderInput::last_synced_at`] and
    /// [`FolderInput::last_attempt_at`] in crates/sync/src/schedule.rs:
    /// both fields are `Option`, and `None` on either one already means
    /// exactly "never" and is handled by `next_sync` as due promptly, not
    /// as an error case. Missing `sync_state` columns are `COALESCE`'d to
    /// the same "never" values here (`NULL`/`0`) so a fresh folder and a
    /// folder with an explicit zero-failure row are indistinguishable to
    /// the scheduler, which is correct: they mean the same thing.
    ///
    /// A folder the provider hasn't discovered yet (no row in `folders` at
    /// all -- e.g. Sent/Drafts/Spam before the sync engine creates them,
    /// per [`Core::list_folders`]'s doc comment) is not returned here
    /// either, but for a different reason than the `LEFT JOIN` above: there
    /// is no `folders.id` to sync against yet, so the scheduler has nothing
    /// it could act on even if it wanted to.
    /// T-132: how far the first-run backfill has got, across every
    /// account and folder that still has one outstanding.
    ///
    /// Owner, on the live profile: «если он постоянно качает, то лучше
    /// сделать аккуратный минималистичный прогрессбар, который будет
    /// показывать весь процесс». The number that answers "how much is
    /// left" is not a message count -- it is the UID window the resumable
    /// backfill still has to walk (`feathermail_sync::FolderSyncState`):
    /// `backfill_target` is where it started, `backfill_floor` is how far
    /// down it has got, and 1 is the bottom.
    ///
    /// `None` means no folder is backfilling: nothing to show, and the
    /// caller stops polling. A folder that finished simply has no row
    /// with a floor left, so it neither pads the total nor holds the bar
    /// at 100 %.
    ///
    /// One statement over `sync_state` (a row per folder, tens of rows on
    /// the owner's profile), no join with `messages` -- this is polled
    /// while a long backfill runs, so it has to stay a table scan of
    /// something tiny.
    pub fn sync_progress(&self) -> Result<Option<SyncProgress>, CoreError> {
        let conn = self.db.conn();
        let mut stmt = conn
            .prepare(
                "SELECT backfill_target, backfill_floor FROM sync_state \
                 WHERE backfill_floor IS NOT NULL AND backfill_target IS NOT NULL",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                let target: i64 = row.get(0)?;
                let floor: i64 = row.get(1)?;
                Ok((target, floor))
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(sync_progress_from_windows(&rows))
    }

    pub fn folder_sync_inputs(&self, account: &AccountId) -> Result<Vec<FolderInput>, CoreError> {
        self.require_account(account.as_str())?;
        let conn = self.db.conn();
        let mut stmt = conn
            .prepare(
                "SELECT f.id, f.kind, s.last_sync_at, s.last_attempt_at, \
                 COALESCE(s.consecutive_failures, 0) \
                 FROM folders f \
                 LEFT JOIN sync_state s ON s.account_id = f.account_id AND s.folder_id = f.id \
                 WHERE f.account_id = ?1 \
                 ORDER BY f.id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![account.as_str()], |row| {
                let id: String = row.get(0)?;
                let kind: String = row.get(1)?;
                let last_synced_at: Option<i64> = row.get(2)?;
                let last_attempt_at: Option<i64> = row.get(3)?;
                let consecutive_failures: i64 = row.get(4)?;
                Ok((
                    id,
                    kind,
                    last_synced_at,
                    last_attempt_at,
                    consecutive_failures,
                ))
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(rows
            .into_iter()
            .map(
                |(id, kind, last_synced_at, last_attempt_at, consecutive_failures)| FolderInput {
                    id,
                    role: folder_role(FolderKind::parse(&kind).unwrap_or(FolderKind::Custom)),
                    last_synced_at,
                    last_attempt_at,
                    consecutive_failures: consecutive_failures.max(0) as u32,
                },
            )
            .collect())
    }

    /// T-078 (b) prep: records one sync attempt against `folder` for the
    /// scheduler's backoff bookkeeping (`sync_state.last_attempt_at` /
    /// `consecutive_failures` -- see schema.sql's doc comment on those
    /// columns, and D32). `last_attempt_at` is set to `now` unconditionally
    /// -- an attempt happened whether or not it succeeded, and that clock
    /// existing at all is what lets backoff delay the *next* try instead of
    /// looking permanently overdue (see
    /// [`FolderInput::last_attempt_at`]'s doc comment for the crash-loop
    /// scenario this prevents). `ok` decides `consecutive_failures`: reset
    /// to `0` on success, incremented on failure.
    ///
    /// UPSERTs rather than requiring a pre-existing row, because the first
    /// attempt against a brand-new folder is exactly when there is no
    /// `sync_state` row yet -- the same "never synced is legitimate" case
    /// [`Core::folder_sync_inputs`] handles on the read side.
    pub fn record_sync_attempt(
        &self,
        account: &AccountId,
        folder: &str,
        ok: bool,
        now: i64,
    ) -> Result<(), CoreError> {
        self.require_account(account.as_str())?;
        // Bound once and reused as the numbered parameter `?4` both in the
        // `INSERT`'s own `VALUES` and inside the `ON CONFLICT` `CASE` --
        // SQLite allows a numbered parameter to be referenced more than
        // once in the same statement, so this is one seed value, not two
        // independent ones that could drift apart.
        let seed_failures: i64 = if ok { 0 } else { 1 };
        self.db
            .conn()
            .execute(
                "INSERT INTO sync_state (account_id, folder_id, last_attempt_at, consecutive_failures)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(account_id, folder_id) DO UPDATE SET
                    last_attempt_at = excluded.last_attempt_at,
                    consecutive_failures = CASE WHEN ?4 = 0 THEN 0 ELSE sync_state.consecutive_failures + 1 END",
                params![account.as_str(), folder, now, seed_failures],
            )
            .map_err(sql_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_sync::{sync_folder, MailboxSession, MailboxSnapshot, UidRange};
    use std::collections::HashMap;

    fn seed_account_and_folder(db: &Database) {
        db.conn()
            .execute(
                "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
                 VALUES ('john', 'John Doe', 'john@example.com', 'generic', 'synced', 'recent', 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO folders (id, account_id, name, kind) VALUES ('inbox', 'john', 'Inbox', 'inbox')",
                [],
            )
            .unwrap();
    }

    /// T-132: the bar shows the whole outstanding download, so two
    /// folders still backfilling are one fraction, not two.
    #[test]
    fn sync_progress_folds_every_outstanding_backfill_into_one_fraction() {
        // Folder A: started at 1001, walked down to 501 -- half of its
        // 1000-UID window. Folder B: started at 101, walked to 51 -- half
        // of its 100.
        let progress =
            sync_progress_from_windows(&[(1001, 501), (101, 51)]).expect("both are still going");
        assert_eq!(progress.total, 1000 + 100);
        assert_eq!(progress.done, 500 + 50);
        assert_eq!(progress.percent(), 50);
    }

    /// A finished folder is not a row with a floor, and the engine also
    /// spells "finished" as a floor of 1 (see `sync_folder`). Neither may
    /// pad the total, and neither may hold the bar on screen.
    #[test]
    fn a_finished_backfill_is_not_progress_to_show() {
        assert_eq!(sync_progress_from_windows(&[]), None);
        assert_eq!(sync_progress_from_windows(&[(9000, 1)]), None);
        assert_eq!(sync_progress_from_windows(&[(1, 1)]), None);
        let mixed = sync_progress_from_windows(&[(9000, 1), (201, 101)])
            .expect("one folder is still going");
        assert_eq!(
            mixed.total, 200,
            "the folder that finished contributes nothing, or the bar sits at a fake 99%"
        );
        assert_eq!(mixed.done, 100);
    }

    /// The fraction is what a `GtkProgressBar` is fed, so it must be a
    /// real number in `0.0..=1.0` even when the arithmetic degenerates.
    #[test]
    fn the_fraction_is_never_nan_and_never_out_of_range() {
        let empty = SyncProgress { done: 0, total: 0 };
        assert!(empty.fraction().is_finite());
        assert_eq!(empty.fraction(), 1.0);
        let over = SyncProgress {
            done: 500,
            total: 100,
        };
        assert_eq!(over.fraction(), 1.0);
        assert_eq!(over.percent(), 100);
        let quarter = SyncProgress {
            done: 25,
            total: 100,
        };
        assert_eq!(quarter.percent(), 25);
    }

    /// The same numbers, read back through the database the worker
    /// actually writes: a folder mid-backfill shows up, and nothing else
    /// is invented.
    #[test]
    fn sync_progress_reads_the_backfill_cursor_from_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let core = Core::open(&path).unwrap();
        seed_account_and_folder(&core.db);
        assert_eq!(
            core.sync_progress().unwrap(),
            None,
            "a profile with nothing backfilling has no bar"
        );
        core.db
            .conn()
            .execute(
                "INSERT INTO sync_state (account_id, folder_id, uidvalidity, backfill_target, backfill_floor)
                 VALUES ('john', 'inbox', 1, 2001, 1001)",
                [],
            )
            .unwrap();
        let progress = core.sync_progress().unwrap().expect("inbox is backfilling");
        assert_eq!(progress.total, 2000);
        assert_eq!(progress.done, 1000);
        assert_eq!(progress.percent(), 50);
    }

    fn header(uid: u32, subject: &str, from: &str, date: &str) -> HeaderMeta {
        HeaderMeta {
            uid,
            flags: vec![],
            internaldate: None,
            size_bytes: Some(100),
            message_id: Some(format!("<m{uid}@x>")),
            in_reply_to: None,
            references: vec![],
            from: Some(from.to_string()),
            to: Some("dest@example.com".to_string()),
            cc: None,
            subject: Some(subject.to_string()),
            date: Some(date.to_string()),
            gm_thrid: None,
        }
    }

    fn seed_real_move_core() -> Core {
        let core = Core::memory().unwrap();
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES ('john', 'John Doe', 'john@example.com', 'generic', 'synced', 'recent', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, remote_id, name, kind)
             VALUES ('inbox', 'john', 'INBOX', 'Inbox', 'inbox')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, remote_id, name, kind)
             VALUES ('archive', 'john', 'Archive', 'Archive', 'archive')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
             VALUES ('t1', 'john', 'inbox', 'Hello', 'Hello', 1704103200, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (
                id, account_id, thread_id, folder_id, provider_uid, message_id_header,
                date, sender_name, sender_email, subject, snippet, unread, size_bytes
             ) VALUES ('m1', 'john', 't1', 'inbox', 7, '<m1@x>', 1704103200,
                       'Sender', 'sender@example.com', 'Hello', 'Hello', 1, 100)",
            [],
        )
        .unwrap();
        core
    }

    fn moved_header(uid: u32) -> HeaderMeta {
        HeaderMeta {
            uid,
            flags: vec!["\\Seen".into()],
            internaldate: None,
            size_bytes: Some(100),
            message_id: Some("<m1@x>".into()),
            in_reply_to: None,
            references: vec![],
            from: Some("Sender <sender@example.com>".into()),
            to: Some("dest@example.com".into()),
            cc: None,
            subject: Some("Hello".into()),
            date: Some("01 Jan 2024 10:00:00 +0000".into()),
            gm_thrid: None,
        }
    }

    #[test]
    fn own_move_rehomes_one_stable_message_in_inbox_then_archive_order() {
        let mut core = seed_real_move_core();
        core.dispatch(crate::command::Command::Archive {
            account_id: crate::model::AccountId("john".into()),
            thread_ids: vec![crate::model::ThreadId("t1".into())],
        })
        .unwrap();
        let row: (String, String, i64, String, String) = core
            .db
            .conn()
            .query_row(
                "SELECT message_id, source_folder_id, source_uid, destination_folder_id, destination_remote_id
                 FROM operation_moves",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "m1".into(),
                "inbox".into(),
                7,
                "archive".into(),
                "Archive".into()
            )
        );

        let located = crate::provider::RemoteLocator::thread_messages(
            &core,
            &crate::model::AccountId("john".into()),
            "t1",
        )
        .unwrap();
        assert_eq!(
            located,
            vec![crate::provider::RemoteMessage {
                folder: "INBOX".into(),
                uid: 7
            }]
        );

        core.db
            .conn()
            .execute("UPDATE operations SET status = 'acked'", [])
            .unwrap();
        let mut source = CoreSyncStore::new(&core.db, "john", "inbox");
        source.remove_vanished("INBOX", &[7]).unwrap();
        let mut destination = CoreSyncStore::new(&core.db, "john", "archive");
        destination
            .upsert_headers("Archive", &[moved_header(42)])
            .unwrap();

        let (count, id, folder, uid, intents): (i64, String, String, i64, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*), MIN(id), MIN(folder_id), MIN(provider_uid),
                        (SELECT COUNT(*) FROM operation_moves)
                 FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            (count, id, folder, uid, intents),
            (1, "m1".into(), "archive".into(), 42, 0)
        );
    }

    #[test]
    fn own_move_rehomes_one_stable_message_when_archive_syncs_first() {
        let mut core = seed_real_move_core();
        core.dispatch(crate::command::Command::Archive {
            account_id: crate::model::AccountId("john".into()),
            thread_ids: vec![crate::model::ThreadId("t1".into())],
        })
        .unwrap();

        let mut destination = CoreSyncStore::new(&core.db, "john", "archive");
        destination
            .upsert_headers("Archive", &[moved_header(42)])
            .unwrap();
        let located = crate::provider::RemoteLocator::thread_messages(
            &core,
            &crate::model::AccountId("john".into()),
            "t1",
        )
        .unwrap();
        assert_eq!(
            located,
            vec![crate::provider::RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
            "destination-first sync must leave the pending provider locator on source"
        );
        core.db
            .conn()
            .execute("UPDATE operations SET status = 'acked'", [])
            .unwrap();
        let mut source = CoreSyncStore::new(&core.db, "john", "inbox");
        source.remove_vanished("INBOX", &[7]).unwrap();

        let (count, id, folder, uid, intents): (i64, String, String, i64, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM messages),
                    (SELECT MIN(id) FROM messages),
                    (SELECT MIN(folder_id) FROM messages),
                    (SELECT MIN(provider_uid) FROM messages),
                    (SELECT COUNT(*) FROM operation_moves)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            (count, id, folder, uid, intents),
            (1, "m1".into(), "archive".into(), 42, 0)
        );
    }

    #[test]
    fn destination_preexisting_message_id_is_merged_without_a_duplicate() {
        let mut core = seed_real_move_core();
        core.db
            .conn()
            .execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
                 VALUES ('stale-thread', 'john', 'archive', 'Hello', 'Hello', 1704103200, 0)",
                [],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO messages (
                    id, account_id, thread_id, folder_id, provider_uid, message_id_header,
                    date, sender_name, sender_email, subject, snippet, unread, size_bytes
                 ) VALUES ('stale-message', 'john', 'stale-thread', 'archive', 99, '<m1@x>',
                           1704103200, 'Sender', 'sender@example.com', 'Hello', 'Hello', 0, 100)",
                [],
            )
            .unwrap();
        core.dispatch(crate::command::Command::Archive {
            account_id: crate::model::AccountId("john".into()),
            thread_ids: vec![crate::model::ThreadId("t1".into())],
        })
        .unwrap();
        core.db
            .conn()
            .execute("UPDATE operations SET status = 'acked'", [])
            .unwrap();
        CoreSyncStore::new(&core.db, "john", "inbox")
            .remove_vanished("INBOX", &[7])
            .unwrap();
        CoreSyncStore::new(&core.db, "john", "archive")
            .upsert_headers("Archive", &[moved_header(42)])
            .unwrap();

        let (messages, threads, intents): (i64, i64, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM messages),
                    (SELECT COUNT(*) FROM threads),
                    (SELECT COUNT(*) FROM operation_moves)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((messages, threads, intents), (1, 1, 0));
    }

    #[test]
    fn parses_rfc2822_and_internaldate() {
        // Cross-checked against `date -u -d "1994-11-15T13:12:31Z" +%s`
        // (08:12:31 -0500 == 13:12:31 UTC).
        assert_eq!(
            parse_rfc2822_date("Tue, 15 Nov 1994 08:12:31 -0500"),
            Some(784905151)
        );
        assert_eq!(
            parse_rfc2822_date("15 Nov 1994 08:12:31 +0000"),
            Some(showtime(1994, 11, 15, 8, 12, 31))
        );
        assert_eq!(
            parse_internaldate("01-Jan-2024 00:00:00 +0000"),
            Some(showtime(2024, 1, 1, 0, 0, 0))
        );
        assert_eq!(parse_rfc2822_date("garbage"), None);
    }

    fn showtime(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
        to_epoch(y, mo, d, h, mi, s, 0)
    }

    #[test]
    fn splits_display_address_variants() {
        assert_eq!(
            split_display_address("Jane Doe <jane@example.com>"),
            ("Jane Doe".to_string(), "jane@example.com".to_string())
        );
        assert_eq!(
            split_display_address("jane@example.com"),
            (String::new(), "jane@example.com".to_string())
        );
        assert_eq!(
            split_display_address("\"Doe, Jane\" <jane@example.com>"),
            ("Doe, Jane".to_string(), "jane@example.com".to_string())
        );
    }

    #[test]
    fn load_state_defaults_when_no_row_yet() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let state = store.load_state("INBOX").unwrap();
        assert_eq!(state, FolderSyncState::default());
    }

    #[test]
    fn save_and_load_round_trips_backfill_columns() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let state = FolderSyncState {
            uidvalidity: Some(7),
            uidnext: Some(50),
            highest_modseq: Some(1000),
            last_synced_at: Some(999),
            backfill_floor: Some(20),
            backfill_target: Some(50),
        };
        store.save_state("INBOX", &state).unwrap();
        let loaded = store.load_state("INBOX").unwrap();
        assert_eq!(loaded, state);

        // Overwrite: save_state must update in place, not duplicate the row.
        let state2 = FolderSyncState {
            backfill_floor: None,
            ..state.clone()
        };
        store.save_state("INBOX", &state2).unwrap();
        assert_eq!(store.load_state("INBOX").unwrap(), state2);
        let rows: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM sync_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_batch_that_fails_partway_leaves_nothing_behind() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);

        // Squat on the thread id uid 4 will deterministically pick, so that
        // header's `INSERT INTO threads` trips the primary key. That is a
        // stand-in for any mid-batch failure; what is under test is what the
        // three headers *before* it leave behind when it happens.
        db.conn()
            .execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
                 VALUES ('thr:john:inbox:4', 'john', 'inbox', 'squatter', '', 0, 0, 0)",
                [],
            )
            .unwrap();

        let batch: Vec<HeaderMeta> = (1..=5)
            .map(|uid| {
                header(
                    uid,
                    "Subject",
                    "Jane Doe <jane@example.com>",
                    "01 Jan 2024 10:00:00 +0000",
                )
            })
            .collect();
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        assert!(store.upsert_headers("INBOX", &batch).is_err());

        // Not "1, 2 and 3 got through" -- the batch either happened or it
        // did not, so the engine's retry re-walks the same range and the
        // counted-in headers cannot drift from the saved floor.
        let messages: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(messages, 0, "partial batch survived the failure");

        // The squatter is pre-existing state, not something this batch
        // wrote: rollback must leave it exactly as it was.
        let threads: Vec<String> = db
            .conn()
            .prepare("SELECT id FROM threads ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(threads, vec!["thr:john:inbox:4".to_string()]);
    }

    #[test]
    fn upsert_headers_inserts_new_message_and_thread() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let h = header(
            7,
            "Hello",
            "Jane Doe <jane@example.com>",
            "01 Jan 2024 10:00:00 +0000",
        );
        store
            .upsert_headers("INBOX", std::slice::from_ref(&h))
            .unwrap();

        let (subject, sender_name, sender_email, unread, thread_id): (
            String,
            String,
            String,
            i64,
            String,
        ) = db
            .conn()
            .query_row(
                "SELECT subject, sender_name, sender_email, unread, thread_id FROM messages WHERE account_id='john' AND folder_id='inbox' AND provider_uid=7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(subject, "Hello");
        assert_eq!(sender_name, "Jane Doe");
        assert_eq!(sender_email, "jane@example.com");
        assert_eq!(unread, 1);

        let thread_subject: String = db
            .conn()
            .query_row(
                "SELECT subject FROM threads WHERE id = ?1",
                params![thread_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(thread_subject, "Hello");
    }

    /// T-046: header sync persists Cc independently from To.  Reply all
    /// reads these two fields as separate RFC recipient groups, so merging
    /// them here would make later self-exclusion and deduplication lossy.
    #[test]
    fn upsert_headers_persists_cc_separately_from_recipients() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let mut h = header(
            7,
            "Hello",
            "Jane Doe <jane@example.com>",
            "01 Jan 2024 10:00:00 +0000",
        );
        h.to = Some("writer@example.com".to_string());
        h.cc = Some("copy@example.com".to_string());
        store
            .upsert_headers("INBOX", std::slice::from_ref(&h))
            .unwrap();

        let row: (String, String) = db
            .conn()
            .query_row(
                "SELECT recipients, cc FROM messages
                 WHERE account_id = 'john' AND folder_id = 'inbox' AND provider_uid = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, "writer@example.com");
        assert_eq!(row.1, "copy@example.com");
    }

    #[test]
    fn upsert_headers_decodes_rfc2047_display_fields_before_persistence() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let h = header(
            7,
            "=?utf-8?B?0KLQtdC80LAg0L/RgNC+0LLQtdGA0LrQsA==?=",
            "=?utf-8?B?0JTQttC10L0=?= <jane@example.com>",
            "01 Jan 2024 10:00:00 +0000",
        );
        store
            .upsert_headers("INBOX", std::slice::from_ref(&h))
            .unwrap();

        let (subject, sender_name, thread_subject): (String, String, String) = db
            .conn()
            .query_row(
                "SELECT m.subject, m.sender_name, t.subject
                 FROM messages m JOIN threads t ON t.id = m.thread_id
                 WHERE m.account_id = 'john' AND m.folder_id = 'inbox' AND m.provider_uid = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(subject, "Тема проверка");
        assert_eq!(sender_name, "Джен");
        assert_eq!(thread_subject, "Тема проверка");
    }

    /// T-048's write side, exercised through the real production call site
    /// (`CoreSyncStore::upsert_one`'s new-message branch), not through a
    /// test helper that enqueues by hand. A message that IMAP sync has
    /// never seen before must land in `fts_pending` so the background
    /// indexer picks it up -- this is the "message appeared" half of the
    /// module doc's "two cases, one queue".
    #[test]
    fn upsert_headers_of_a_new_message_queues_it_for_background_indexing() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let h = header(
            7,
            "Hello",
            "Jane Doe <jane@example.com>",
            "01 Jan 2024 10:00:00 +0000",
        );
        store
            .upsert_headers("INBOX", std::slice::from_ref(&h))
            .unwrap();

        let message_id: String = db
            .conn()
            .query_row(
                "SELECT id FROM messages WHERE account_id='john' AND folder_id='inbox' AND provider_uid=7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let pending: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM fts_pending WHERE message_id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            pending, 1,
            "a brand-new message must be queued for background indexing"
        );
    }

    /// The acceptance-critical regression: a flags-only [`HeaderMeta`]
    /// (every field but `uid`/`flags` left at its Rust default, exactly
    /// what a CONDSTORE `CHANGEDSINCE` delta or plain flags re-fetch sends)
    /// must merge into the already-stored row, not blank out the
    /// subject/from/date a full header fetch already wrote.
    #[test]
    fn flags_only_upsert_does_not_clobber_previously_saved_headers() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let full = header(
            7,
            "Original Subject",
            "Jane Doe <jane@example.com>",
            "01 Jan 2024 10:00:00 +0000",
        );
        store
            .upsert_headers("INBOX", std::slice::from_ref(&full))
            .unwrap();

        let flags_only = HeaderMeta {
            uid: 7,
            flags: vec!["\\Seen".to_string(), "\\Flagged".to_string()],
            ..HeaderMeta::default()
        };
        store
            .upsert_headers("INBOX", std::slice::from_ref(&flags_only))
            .unwrap();

        let (subject, sender_name, sender_email, unread, starred): (String, String, String, i64, i64) = db
            .conn()
            .query_row(
                "SELECT subject, sender_name, sender_email, unread, starred FROM messages WHERE account_id='john' AND folder_id='inbox' AND provider_uid=7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            subject, "Original Subject",
            "flags-only update must not clobber subject"
        );
        assert_eq!(sender_name, "Jane Doe");
        assert_eq!(sender_email, "jane@example.com");
        assert_eq!(unread, 0, "\\Seen present now -> read");
        assert_eq!(starred, 1, "\\Flagged now present");
    }

    /// The other acceptance-critical regression: `remove_vanished` must not
    /// leave a `messages_fts` row behind for a message it deletes.
    #[test]
    fn remove_vanished_deletes_message_and_its_fts_row() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let h = header(7, "Hello", "jane@example.com", "01 Jan 2024 10:00:00 +0000");
        store
            .upsert_headers("INBOX", std::slice::from_ref(&h))
            .unwrap();

        let msg_id: String = db
            .conn()
            .query_row(
                "SELECT id FROM messages WHERE account_id='john' AND folder_id='inbox' AND provider_uid=7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Simulate a search index entry for this message (whatever future
        // writer populates `messages_fts` -- this adapter itself does not,
        // headers-only sync has no body/snippet text to index yet).
        db.conn()
            .execute(
                "INSERT INTO messages_fts (sender, recipients, subject, body, attachment_names, labels, message_id)
                 VALUES ('jane@example.com', 'dest@example.com', 'Hello', '', '', '', ?1)",
                params![msg_id],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO fts_message_rows (message_id, fts_rowid) VALUES (?1, ?2)",
                params![msg_id, db.conn().last_insert_rowid()],
            )
            .unwrap();

        store.remove_vanished("INBOX", &[7]).unwrap();

        let messages_left: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE account_id='john' AND folder_id='inbox'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(messages_left, 0);
        let fts_left: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE message_id = ?1",
                params![msg_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            fts_left, 0,
            "messages_fts must not leak a vanished message's row"
        );
        let threads_left: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE account_id='john'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(threads_left, 0, "the now-orphaned thread must go too");
    }

    #[test]
    fn reset_folder_wipes_messages_and_threads_for_that_folder_only() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        db.conn()
            .execute(
                "INSERT INTO folders (id, account_id, name, kind) VALUES ('sent', 'john', 'Sent', 'sent')",
                [],
            )
            .unwrap();
        let mut inbox_store = CoreSyncStore::new(&db, "john", "inbox");
        inbox_store
            .upsert_headers(
                "INBOX",
                std::slice::from_ref(&header(1, "A", "a@x.com", "01 Jan 2024 00:00:00 +0000")),
            )
            .unwrap();
        let mut sent_store = CoreSyncStore::new(&db, "john", "sent");
        sent_store
            .upsert_headers(
                "Sent",
                std::slice::from_ref(&header(1, "B", "b@x.com", "01 Jan 2024 00:00:00 +0000")),
            )
            .unwrap();

        inbox_store.reset_folder("INBOX").unwrap();

        let inbox_left: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE folder_id='inbox'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inbox_left, 0);
        let sent_left: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE folder_id='sent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sent_left, 1, "reset_folder must not touch other folders");
    }

    /// End-to-end acceptance test: `sync_folder` driven by a real
    /// `CoreSyncStore` over an in-memory `Database`. A fake, in-process
    /// `MailboxSession` stands in for IMAP (a real socket-based one is
    /// exercised in `feathermail-providers`); what this test is actually
    /// checking is the store side -- that a second run against the same
    /// database only pulls the delta, exactly like the pure-engine test of
    /// the same name in `feathermail-sync`, but now through real SQL.
    struct FakeSession {
        messages: Vec<HeaderMeta>,
        uidvalidity: u32,
    }

    impl MailboxSession for FakeSession {
        fn select(&mut self, _folder: &str) -> Result<MailboxSnapshot, SyncError> {
            let uidnext = self
                .messages
                .iter()
                .map(|m| m.uid)
                .max()
                .map_or(1, |m| m + 1);
            Ok(MailboxSnapshot {
                uidvalidity: self.uidvalidity,
                uidnext,
                exists: self.messages.len() as u32,
                highest_modseq: None,
            })
        }

        fn uid_fetch_headers(
            &mut self,
            _folder: &str,
            range: UidRange,
        ) -> Result<Vec<HeaderMeta>, SyncError> {
            let to = range.to.unwrap_or(u32::MAX);
            Ok(self
                .messages
                .iter()
                .filter(|m| m.uid >= range.from && m.uid <= to)
                .cloned()
                .collect())
        }

        fn uid_fetch_flags_changed_since(
            &mut self,
            _folder: &str,
            _range: UidRange,
            _modseq: u64,
        ) -> Result<Vec<HeaderMeta>, SyncError> {
            Ok(vec![])
        }

        fn list_folders(&mut self) -> Result<Vec<String>, SyncError> {
            Ok(vec!["INBOX".into()])
        }

        fn fetch_body(&mut self, _folder: &str, _uid: u32) -> Result<Vec<u8>, SyncError> {
            // Not exercised by this file's tests (they cover the header
            // sync path only) -- present only so `FakeSession` still
            // satisfies `MailboxSession` after T-024 added this method.
            Err(SyncError::Session("FakeSession has no bodies".into()))
        }
    }

    #[test]
    fn sync_folder_over_real_sqlite_store_second_run_fetches_only_delta() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let mut session = FakeSession {
            messages: (1..=10)
                .map(|uid| header(uid, "Subj", "a@b.com", "01 Jan 2024 00:00:00 +0000"))
                .collect(),
            uidvalidity: 1,
        };
        let cancel = || false;

        let out1 = sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert_eq!(out1.headers_fetched, 10);
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 10);

        session
            .messages
            .push(header(11, "New", "c@d.com", "02 Jan 2024 00:00:00 +0000"));
        session
            .messages
            .push(header(12, "New 2", "e@f.com", "02 Jan 2024 00:00:00 +0000"));

        let out2 = sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert_eq!(
            out2.headers_fetched, 2,
            "second run must only pull the delta"
        );
        let count2: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count2, 12);

        let out3 = sync_folder(&mut session, &mut store, "INBOX", 3, &cancel).unwrap();
        assert_eq!(out3.headers_fetched, 0, "third, no-op run fetches nothing");

        // A HashMap keyed by uid, built from the table, confirms no
        // duplicate rows and every uid present exactly once.
        let mut by_uid: HashMap<i64, i64> = HashMap::new();
        let mut stmt = db
            .conn()
            .prepare("SELECT provider_uid, COUNT(*) FROM messages GROUP BY provider_uid")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .unwrap();
        for row in rows {
            let (uid, n) = row.unwrap();
            by_uid.insert(uid, n);
        }
        assert_eq!(by_uid.len(), 12);
        assert!(by_uid.values().all(|&n| n == 1));
    }

    // --- T-027: duplicate Message-Id / corrupted RFC822 fixtures ----------
    //
    // The wire-level RFC822/IMAP parsing that would actually produce these
    // shapes from raw server bytes lives in `feathermail-providers`, out of
    // this ticket's file scope. These fixtures stand in for what that
    // (lenient, never-panicking) parser hands off once it gives up on a
    // garbled header block: a `HeaderMeta` that is technically well-typed
    // but carries little or no usable content, or a `Date:` string that
    // parses as a numeral but is not a sane calendar date.

    /// What a totally garbled header block degrades to: nothing recognized
    /// at all beyond the UID IMAP itself reported.
    fn garbage_header(uid: u32) -> HeaderMeta {
        HeaderMeta {
            uid,
            flags: vec![],
            ..HeaderMeta::default()
        }
    }

    #[test]
    fn parse_rfc2822_date_rejects_an_absurd_year_instead_of_overflowing() {
        // Before the T-027 fix this overflowed `i64` arithmetic in
        // `to_epoch` (`days * 86400`) and panicked in a debug/test build.
        assert_eq!(
            parse_rfc2822_date("01 Jan 999999999999 00:00:00 +0000"),
            None
        );
        assert_eq!(
            parse_rfc2822_date("01 Jan -999999999999 00:00:00 +0000"),
            None
        );
        // A sane year on either side of the accepted range still works.
        assert!(parse_rfc2822_date("01 Jan 2024 00:00:00 +0000").is_some());
    }

    #[test]
    fn parse_internaldate_rejects_an_absurd_year_instead_of_overflowing() {
        assert_eq!(
            parse_internaldate("01-Jan-999999999999 00:00:00 +0000"),
            None
        );
        assert!(parse_internaldate("01-Jan-2024 00:00:00 +0000").is_some());
    }

    /// The acceptance scenario from the ticket: in a batch of N headers, one
    /// carries an absurd `Date:` year (the crash vector fixed above) mixed
    /// in among ordinary messages. The whole batch must still upsert
    /// (through the real public entry point, not the private parser)
    /// without error, and the corrupted message must land with a safe
    /// fallback date rather than wrapped-arithmetic garbage.
    #[test]
    fn batch_with_one_absurd_date_header_does_not_crash_and_the_rest_still_land() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");

        let mut batch: Vec<HeaderMeta> = (1..=5)
            .map(|uid| header(uid, "Subject", "a@b.com", "01 Jan 2024 10:00:00 +0000"))
            .collect();
        batch[2].date = Some("01 Jan 999999999999 00:00:00 +0000".to_string());
        batch[2].internaldate = None;

        store.upsert_headers("INBOX", &batch).unwrap();

        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 5, "all five messages must land, corrupted or not");

        let (date, subject): (i64, String) = db
            .conn()
            .query_row(
                "SELECT date, subject FROM messages WHERE provider_uid = 3",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            date, 0,
            "an unparseable date must fall back to 0, not garbage"
        );
        assert_eq!(
            subject, "Subject",
            "a bad date alone must not blank out an otherwise-good subject"
        );
    }

    /// A message whose headers carry nothing at all -- no Message-Id, no
    /// From, no Subject, and no parseable date from either `Date:` or
    /// INTERNALDATE -- must still appear in the list, marked as
    /// unparseable, rather than vanish silently or crash the batch.
    #[test]
    fn brand_new_message_with_no_usable_headers_is_marked_unable_to_display() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");

        let mut batch: Vec<HeaderMeta> = (1..=4)
            .map(|uid| header(uid, "Subject", "a@b.com", "01 Jan 2024 10:00:00 +0000"))
            .collect();
        batch.push(garbage_header(5));

        store.upsert_headers("INBOX", &batch).unwrap();

        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 5, "the garbage message must not be dropped");

        let (msg_subject, thread_id): (String, String) = db
            .conn()
            .query_row(
                "SELECT subject, thread_id FROM messages WHERE provider_uid = 5",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(msg_subject, "Unable to display");
        let thread_subject: String = db
            .conn()
            .query_row(
                "SELECT subject FROM threads WHERE id = ?1",
                params![thread_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            thread_subject, "Unable to display",
            "the list reads from threads, so the marker must land there too"
        );

        // The four ordinary messages in the same batch must be entirely
        // unaffected by the corrupted one.
        let ok_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE subject = 'Subject'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ok_count, 4);
    }

    /// A legitimate flags-only CONDSTORE delta for an *already-synced*
    /// message is, byte-for-byte, exactly the same "everything but uid/flags
    /// is default" shape as the garbage fixture above. It must never be
    /// mistaken for corruption -- it can't be, because it always lands on
    /// the `exists` branch, but this pins that down through the public
    /// entry point rather than trusting the reasoning alone.
    #[test]
    fn flags_only_update_of_an_existing_message_is_never_treated_as_unparseable() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        store
            .upsert_headers(
                "INBOX",
                std::slice::from_ref(&header(
                    9,
                    "Real Subject",
                    "a@b.com",
                    "01 Jan 2024 00:00:00 +0000",
                )),
            )
            .unwrap();

        let flags_only = HeaderMeta {
            uid: 9,
            flags: vec!["\\Seen".to_string()],
            ..HeaderMeta::default()
        };
        store
            .upsert_headers("INBOX", std::slice::from_ref(&flags_only))
            .unwrap();

        let subject: String = db
            .conn()
            .query_row(
                "SELECT subject FROM messages WHERE provider_uid = 9",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(subject, "Real Subject");
    }

    /// Once a corrected, full header fetch arrives for a UID previously
    /// stored as unparseable (e.g. a later full refetch, or a server that
    /// eventually serves the real headers), the marker must be replaced,
    /// not stuck forever.
    #[test]
    fn corrupted_message_recovers_once_real_headers_arrive_later() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        store
            .upsert_headers("INBOX", std::slice::from_ref(&garbage_header(11)))
            .unwrap();

        let fixed = header(11, "Recovered", "a@b.com", "01 Jan 2024 00:00:00 +0000");
        store
            .upsert_headers("INBOX", std::slice::from_ref(&fixed))
            .unwrap();

        let subject: String = db
            .conn()
            .query_row(
                "SELECT subject FROM messages WHERE provider_uid = 11",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(subject, "Recovered");
    }

    /// Same Message-Id on two UIDs in one folder is one conversation
    /// (D22). The same Message-Id in a second folder stays a second
    /// thread — grouping is folder-local, not cross-folder.
    #[test]
    fn duplicate_message_id_across_two_uids_does_not_crash_or_collide() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        db.conn()
            .execute(
                "INSERT INTO folders (id, account_id, name, kind) VALUES ('sent', 'john', 'Sent', 'sent')",
                [],
            )
            .unwrap();
        let mut inbox = CoreSyncStore::new(&db, "john", "inbox");

        let shared_id = "<dup@example.com>";
        let a = HeaderMeta {
            message_id: Some(shared_id.to_string()),
            ..header(1, "Copy A", "a@b.com", "01 Jan 2024 00:00:00 +0000")
        };
        let b = HeaderMeta {
            message_id: Some(shared_id.to_string()),
            ..header(2, "Copy B", "a@b.com", "01 Jan 2024 00:00:00 +0000")
        };
        inbox.upsert_headers("INBOX", &[a, b]).unwrap();

        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE message_id_header = ?1 AND folder_id = 'inbox'",
                params![shared_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "both copies must be stored, independently");
        let inbox_threads: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(DISTINCT thread_id) FROM messages WHERE folder_id = 'inbox'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            inbox_threads, 1,
            "two UIDs in one folder with one Message-Id are one thread"
        );

        let mut sent = CoreSyncStore::new(&db, "john", "sent");
        let c = HeaderMeta {
            message_id: Some(shared_id.to_string()),
            ..header(1, "Copy C", "a@b.com", "01 Jan 2024 00:00:00 +0000")
        };
        sent.upsert_headers("Sent", std::slice::from_ref(&c))
            .unwrap();
        let folder_threads: i64 = db
            .conn()
            .query_row("SELECT COUNT(DISTINCT thread_id) FROM messages", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            folder_threads, 2,
            "the same Message-Id in a second folder is a second thread"
        );
    }

    /// The server repeating the same UID within one `UID FETCH` response
    /// (a real-world server bug, or a retried/duplicated response) must
    /// upsert idempotently: exactly one row survives, not a crash and not
    /// two rows racing for the same primary key.
    #[test]
    fn repeated_uid_within_one_batch_upserts_idempotently() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");

        let first = header(4, "First", "a@b.com", "01 Jan 2024 00:00:00 +0000");
        let repeat = header(4, "Repeat", "a@b.com", "02 Jan 2024 00:00:00 +0000");
        store.upsert_headers("INBOX", &[first, repeat]).unwrap();

        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE provider_uid = 4",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "one server-side uid must never become two rows");
        let subject: String = db
            .conn()
            .query_row(
                "SELECT subject FROM messages WHERE provider_uid = 4",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(subject, "Repeat", "the later entry in the batch wins");
    }

    /// End-to-end acceptance test for T-027, driven through `sync_folder`
    /// (the actual public entry point) rather than `upsert_headers`
    /// directly: a folder of 5 messages where one is corrupted must still
    /// sync to completion, the cursor must still advance correctly, and a
    /// second pass must neither lose nor duplicate anything.
    #[test]
    fn sync_folder_completes_with_one_corrupted_message_and_a_repeat_run_is_stable() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let mut messages: Vec<HeaderMeta> = (1..=5)
            .map(|uid| header(uid, "Subj", "a@b.com", "01 Jan 2024 00:00:00 +0000"))
            .collect();
        messages[2] = garbage_header(3);
        let mut session = FakeSession {
            messages,
            uidvalidity: 1,
        };
        let cancel = || false;

        let out1 =
            feathermail_sync::sync_folder(&mut session, &mut store, "INBOX", 1, &cancel).unwrap();
        assert!(!out1.cancelled);
        assert_eq!(out1.headers_fetched, 5);

        let count1: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count1, 5, "sync must reach the end of the batch");

        let subject3: String = db
            .conn()
            .query_row(
                "SELECT subject FROM messages WHERE provider_uid = 3",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(subject3, "Unable to display");

        let out2 =
            feathermail_sync::sync_folder(&mut session, &mut store, "INBOX", 2, &cancel).unwrap();
        assert_eq!(
            out2.headers_fetched, 0,
            "a repeat pass must not re-fetch or duplicate anything"
        );
        let count2: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count2, 5, "no loss, no duplication on the repeat run");
    }

    fn john() -> AccountId {
        AccountId("john".into())
    }

    /// T-078 (b) prep: a folder with no `sync_state` row at all yet -- the
    /// common case for a folder the sync engine has never touched -- must
    /// still come back as a `FolderInput`, with the "never" fields reading
    /// `None`/`0` rather than the folder being dropped from the list. This
    /// is the `LEFT JOIN` behavior `folder_sync_inputs`'s doc comment
    /// argues for; a mutation to an `INNER JOIN` would make this folder
    /// vanish silently instead of failing loudly, so it needs its own test.
    #[test]
    fn folder_sync_inputs_includes_never_synced_folder() {
        let core = Core::memory().unwrap();
        seed_account_and_folder(&core.db);
        let inputs = core.folder_sync_inputs(&john()).unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].id, "inbox");
        assert_eq!(inputs[0].role, FolderRole::Inbox);
        assert_eq!(inputs[0].last_synced_at, None);
        assert_eq!(inputs[0].last_attempt_at, None);
        assert_eq!(inputs[0].consecutive_failures, 0);
    }

    /// Every [`FolderKind`] this crate's `folders.kind` can actually hold
    /// maps to *some* [`FolderRole`], and a folder that already has a
    /// `sync_state` row reports its real, non-default values -- not the
    /// "never synced" defaults from the test above.
    #[test]
    fn folder_sync_inputs_maps_kind_to_role_and_reads_existing_state() {
        let core = Core::memory().unwrap();
        seed_account_and_folder(&core.db); // 'inbox', kind 'inbox'
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, name, kind) VALUES ('sent', 'john', 'Sent', 'sent')",
                [],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, name, kind) VALUES ('work', 'john', 'Work', 'custom')",
                [],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO sync_state (account_id, folder_id, last_sync_at, last_attempt_at, consecutive_failures)
                 VALUES ('john', 'inbox', 100, 90, 3)",
                [],
            )
            .unwrap();

        let mut inputs = core.folder_sync_inputs(&john()).unwrap();
        inputs.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(inputs.len(), 3);

        let inbox = inputs.iter().find(|f| f.id == "inbox").unwrap();
        assert_eq!(inbox.role, FolderRole::Inbox);
        assert_eq!(inbox.last_synced_at, Some(100));
        assert_eq!(inbox.last_attempt_at, Some(90));
        assert_eq!(inbox.consecutive_failures, 3);

        let sent = inputs.iter().find(|f| f.id == "sent").unwrap();
        assert_eq!(sent.role, FolderRole::Sent);
        assert_eq!(sent.last_synced_at, None);

        let work = inputs.iter().find(|f| f.id == "work").unwrap();
        assert_eq!(work.role, FolderRole::Other);
    }

    #[test]
    fn folder_sync_inputs_rejects_unknown_account() {
        let core = Core::memory().unwrap();
        let err = core.folder_sync_inputs(&john()).unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::AccountNotFound);
    }

    /// [`Core::record_sync_attempt`] must be able to create the
    /// `sync_state` row itself (T-078 (b) prep) -- the first attempt
    /// against a folder is exactly when no row exists yet, mirroring
    /// `folder_sync_inputs`'s read-side "never synced" case.
    #[test]
    fn record_sync_attempt_upserts_a_missing_row_on_failure() {
        let core = Core::memory().unwrap();
        seed_account_and_folder(&core.db);
        core.record_sync_attempt(&john(), "inbox", false, 500)
            .unwrap();
        let (last_attempt_at, consecutive_failures): (i64, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT last_attempt_at, consecutive_failures FROM sync_state \
                 WHERE account_id = 'john' AND folder_id = 'inbox'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(last_attempt_at, 500);
        assert_eq!(consecutive_failures, 1);
    }

    /// A run of failures must keep incrementing, not reset or saturate at
    /// 1 -- this is the counter D32's backoff is keyed on.
    #[test]
    fn record_sync_attempt_increments_across_repeated_failures() {
        let core = Core::memory().unwrap();
        seed_account_and_folder(&core.db);
        core.record_sync_attempt(&john(), "inbox", false, 100)
            .unwrap();
        core.record_sync_attempt(&john(), "inbox", false, 200)
            .unwrap();
        core.record_sync_attempt(&john(), "inbox", false, 300)
            .unwrap();
        let (last_attempt_at, consecutive_failures): (i64, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT last_attempt_at, consecutive_failures FROM sync_state \
                 WHERE account_id = 'john' AND folder_id = 'inbox'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(last_attempt_at, 300);
        assert_eq!(consecutive_failures, 3);
    }

    /// A success must reset `consecutive_failures` to `0` even after a
    /// run of prior failures -- this is what lets a recovered folder pass
    /// the backoff threshold immediately again on its next real attempt
    /// (see `FolderInput::consecutive_failures`'s doc comment: `0` fully
    /// disables the backoff gate).
    #[test]
    fn record_sync_attempt_resets_failures_on_success() {
        let core = Core::memory().unwrap();
        seed_account_and_folder(&core.db);
        core.record_sync_attempt(&john(), "inbox", false, 100)
            .unwrap();
        core.record_sync_attempt(&john(), "inbox", false, 200)
            .unwrap();
        core.record_sync_attempt(&john(), "inbox", true, 300)
            .unwrap();
        let (last_attempt_at, consecutive_failures): (i64, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT last_attempt_at, consecutive_failures FROM sync_state \
                 WHERE account_id = 'john' AND folder_id = 'inbox'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(last_attempt_at, 300);
        assert_eq!(consecutive_failures, 0);
    }

    #[test]
    fn record_sync_attempt_rejects_unknown_account() {
        let core = Core::memory().unwrap();
        let err = core
            .record_sync_attempt(&john(), "inbox", true, 1)
            .unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::AccountNotFound);
    }

    /// [`Core::sync_store`] is the seam T-078 (b) exists for: it must hand
    /// back a [`CoreSyncStore`] that reads and writes the *same* database
    /// `Core` itself holds, not a disconnected copy -- otherwise
    /// `crates/service` could run `sync_folder` against it all day and the
    /// rest of `Core` (and the scheduler bookkeeping above) would never see
    /// the result.
    #[test]
    fn sync_store_reads_and_writes_cores_own_database() {
        let core = Core::memory().unwrap();
        seed_account_and_folder(&core.db);
        let mut store = core.sync_store(&john(), "inbox");
        store
            .save_state(
                "INBOX",
                &FolderSyncState {
                    uidvalidity: Some(7),
                    uidnext: Some(42),
                    last_synced_at: Some(123),
                    ..Default::default()
                },
            )
            .unwrap();
        drop(store);

        let last_sync_at: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT last_sync_at FROM sync_state WHERE account_id = 'john' AND folder_id = 'inbox'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(last_sync_at, 123);

        // And the other direction: a second store over the same folder
        // sees what the first one just wrote, through Core's one database.
        let mut store2 = core.sync_store(&john(), "inbox");
        let state = store2.load_state("INBOX").unwrap();
        assert_eq!(state.last_synced_at, Some(123));
        assert_eq!(state.uidvalidity, Some(7));
    }

    fn thread_count(db: &Database) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap()
    }

    fn distinct_thread_ids(db: &Database) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(DISTINCT thread_id) FROM messages", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    fn thread_rollup(db: &Database) -> (i64, i64, String) {
        db.conn()
            .query_row(
                "SELECT message_count, unread, subject FROM threads LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    fn thread_id_for_uid(db: &Database, uid: u32) -> String {
        db.conn()
            .query_row(
                "SELECT thread_id FROM messages WHERE provider_uid = ?1",
                params![uid],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn unread_flags(db: &Database, thread_id: &str) -> (i64, i64) {
        let thread: i64 = db
            .conn()
            .query_row(
                "SELECT unread FROM threads WHERE id = ?1",
                params![thread_id],
                |r| r.get(0),
            )
            .unwrap();
        let messages: i64 = db
            .conn()
            .query_row(
                "SELECT MAX(unread) FROM messages WHERE thread_id = ?1",
                params![thread_id],
                |r| r.get(0),
            )
            .unwrap();
        (thread, messages)
    }

    /// Production path (T-029): A←B←C via In-Reply-To through
    /// `upsert_headers`, not the pure helper. Mutation: INSERT that keeps
    /// `thread_row_id(uid)` as the final assignment leaves count=3 threads.
    #[test]
    fn jwz_in_reply_to_chain_through_upsert_is_one_thread() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let a = HeaderMeta {
            message_id: Some("<a@x>".into()),
            ..header(1, "Root", "a@b.com", "01 Jan 2024 10:00:00 +0000")
        };
        let b = HeaderMeta {
            message_id: Some("<b@x>".into()),
            in_reply_to: Some("<a@x>".into()),
            ..header(2, "Re: Root", "a@b.com", "01 Jan 2024 11:00:00 +0000")
        };
        let c = HeaderMeta {
            message_id: Some("<c@x>".into()),
            in_reply_to: Some("<b@x>".into()),
            ..header(3, "Re: Root", "a@b.com", "01 Jan 2024 12:00:00 +0000")
        };
        store.upsert_headers("INBOX", &[a, b, c]).unwrap();
        assert_eq!(distinct_thread_ids(&db), 1);
        assert_eq!(thread_count(&db), 1);
        let (count, _, subject) = thread_rollup(&db);
        assert_eq!(count, 3);
        assert_eq!(subject, "Re: Root", "rollup subject is the latest message");
    }

    #[test]
    fn fifty_headers_in_a_reply_chain_upsert_to_one_thread() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let batch: Vec<HeaderMeta> = (1..=50)
            .map(|uid| {
                let mut h = header(uid, "Chain", "a@b.com", "01 Jan 2024 10:00:00 +0000");
                h.message_id = Some(format!("<m{uid}@x>"));
                if uid > 1 {
                    h.in_reply_to = Some(format!("<m{}@x>", uid - 1));
                }
                h
            })
            .collect();
        store.upsert_headers("INBOX", &batch).unwrap();
        assert_eq!(distinct_thread_ids(&db), 1);
        let (count, _, _) = thread_rollup(&db);
        assert_eq!(count, 50);
    }

    /// Reply lands first; parent in a later batch must merge. Mutation:
    /// assignment only on INSERT of the new row (ignoring existing
    /// children) leaves two threads.
    #[test]
    fn parent_arriving_in_a_later_batch_merges_with_existing_reply() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let reply = HeaderMeta {
            message_id: Some("<b@x>".into()),
            in_reply_to: Some("<a@x>".into()),
            ..header(2, "Re: Root", "a@b.com", "01 Jan 2024 11:00:00 +0000")
        };
        store
            .upsert_headers("INBOX", std::slice::from_ref(&reply))
            .unwrap();
        assert_eq!(distinct_thread_ids(&db), 1);
        let parent = HeaderMeta {
            message_id: Some("<a@x>".into()),
            ..header(1, "Root", "a@b.com", "01 Jan 2024 10:00:00 +0000")
        };
        store
            .upsert_headers("INBOX", std::slice::from_ref(&parent))
            .unwrap();
        assert_eq!(distinct_thread_ids(&db), 1);
        assert_eq!(thread_count(&db), 1);
        let (count, _, _) = thread_rollup(&db);
        assert_eq!(count, 2);
    }

    #[test]
    fn gm_thrid_groups_messages_even_when_references_disagree() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let a = HeaderMeta {
            message_id: Some("<a@x>".into()),
            references: vec!["<other-1@x>".into()],
            gm_thrid: Some("999".into()),
            ..header(1, "A", "a@b.com", "01 Jan 2024 10:00:00 +0000")
        };
        let b = HeaderMeta {
            message_id: Some("<b@x>".into()),
            references: vec!["<other-2@x>".into()],
            gm_thrid: Some("999".into()),
            ..header(2, "B", "a@b.com", "01 Jan 2024 11:00:00 +0000")
        };
        store.upsert_headers("INBOX", &[a, b]).unwrap();
        assert_eq!(distinct_thread_ids(&db), 1);
        let (count, _, _) = thread_rollup(&db);
        assert_eq!(count, 2);
    }

    /// Rollup unread is OR across members, not the latest message's flag.
    /// Mutation: copy the latest row's unread leaves the thread read.
    #[test]
    fn thread_unread_is_or_across_members_not_the_latest() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let unread = HeaderMeta {
            message_id: Some("<a@x>".into()),
            flags: vec![],
            ..header(1, "Root", "a@b.com", "01 Jan 2024 10:00:00 +0000")
        };
        let read_b = HeaderMeta {
            message_id: Some("<b@x>".into()),
            in_reply_to: Some("<a@x>".into()),
            flags: vec!["\\Seen".into()],
            ..header(2, "Re", "a@b.com", "01 Jan 2024 11:00:00 +0000")
        };
        let read_c = HeaderMeta {
            message_id: Some("<c@x>".into()),
            in_reply_to: Some("<b@x>".into()),
            flags: vec!["\\Seen".into()],
            ..header(3, "Re", "a@b.com", "01 Jan 2024 12:00:00 +0000")
        };
        store
            .upsert_headers("INBOX", &[unread, read_b, read_c])
            .unwrap();
        let (count, unread_flag, _) = thread_rollup(&db);
        assert_eq!(count, 3);
        assert_eq!(unread_flag, 1, "one unread member keeps the thread unread");
    }

    /// Pending Archive on an absorbed thread id must follow the survivor.
    /// Mutation: merge messages without UPDATE operations leaves the
    /// queue pointing at a deleted id.
    #[test]
    fn pending_archive_on_absorbed_id_retargets_to_survivor() {
        let mut core = Core::memory().unwrap();
        seed_account_and_folder(&core.db);
        let mut store = CoreSyncStore::new(&core.db, "john", "inbox");
        let reply = HeaderMeta {
            message_id: Some("<b@x>".into()),
            in_reply_to: Some("<a@x>".into()),
            ..header(2, "Re: Root", "a@b.com", "01 Jan 2024 11:00:00 +0000")
        };
        store
            .upsert_headers("INBOX", std::slice::from_ref(&reply))
            .unwrap();
        let absorbed: String = core
            .db
            .conn()
            .query_row(
                "SELECT thread_id FROM messages WHERE provider_uid = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(store);
        core.dispatch(crate::command::Command::Archive {
            account_id: john(),
            thread_ids: vec![crate::model::ThreadId(absorbed.clone())],
        })
        .unwrap();
        let parent = HeaderMeta {
            message_id: Some("<a@x>".into()),
            ..header(1, "Root", "a@b.com", "01 Jan 2024 10:00:00 +0000")
        };
        let mut store = CoreSyncStore::new(&core.db, "john", "inbox");
        store
            .upsert_headers("INBOX", std::slice::from_ref(&parent))
            .unwrap();
        let survivor: String = core
            .db
            .conn()
            .query_row(
                "SELECT thread_id FROM messages WHERE provider_uid = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(
            absorbed, survivor,
            "parent's thread is the lex-min survivor"
        );
        let target: String = core
            .db
            .conn()
            .query_row(
                "SELECT target_id FROM operations WHERE op = 'archive' AND status = 'pending'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(target, survivor);
        let absorbed_left: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE id = ?1",
                params![absorbed],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(absorbed_left, 0);
    }

    #[test]
    fn list_thread_messages_returns_metadata_oldest_first_without_bodies() {
        let core = Core::memory().unwrap();
        seed_account_and_folder(&core.db);
        let mut store = CoreSyncStore::new(&core.db, "john", "inbox");
        let canary = "SECRET_BODY_CANARY_RFC822";
        let a = HeaderMeta {
            message_id: Some("<a@x>".into()),
            ..header(1, "Root", "a@b.com", "01 Jan 2024 10:00:00 +0000")
        };
        let b = HeaderMeta {
            message_id: Some("<b@x>".into()),
            in_reply_to: Some("<a@x>".into()),
            ..header(2, canary, "a@b.com", "01 Jan 2024 11:00:00 +0000")
        };
        store.upsert_headers("INBOX", &[a, b]).unwrap();
        let tid: String = core
            .db
            .conn()
            .query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap();
        let msgs = core
            .list_thread_messages(&john(), &crate::model::ThreadId(tid.clone()))
            .unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].subject, "Root");
        assert_eq!(msgs[1].subject, canary);
        assert!(msgs[0].date <= msgs[1].date);
        let card = core
            .get_thread(&john(), &crate::model::ThreadId(tid))
            .unwrap();
        assert_eq!(card.message_count, 2);
        assert_eq!(
            card.message_id.as_ref().map(|m| m.as_str()),
            Some(msgs[1].id.as_str()),
            "get_thread stays the list card pointing at the latest message"
        );
        let debug = format!("{:?}", msgs[1]);
        assert!(
            !debug.contains(canary),
            "ThreadMessage Debug must not print subject/body/RFC822: {debug}"
        );
        assert!(
            !debug.contains("raw:"),
            "ThreadMessage Debug must not grow a raw field: {debug}"
        );
    }

    #[test]
    fn core_open_rethreads_existing_one_to_one_mail_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let db = Database::open(&path).unwrap();
            seed_account_and_folder(&db);
            for (uid, mid, irt, subj) in [
                (1_u32, "<a@x>", None, "Root"),
                (2, "<b@x>", Some("<a@x>"), "Re"),
                (3, "<c@x>", Some("<b@x>"), "Re"),
            ] {
                let tid = format!("thr:john:inbox:{uid}");
                let mid_s = mid;
                db.conn()
                    .execute(
                        "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
                         VALUES (?1, 'john', 'inbox', ?2, '', ?3, 1, 0)",
                        params![tid, subj, 1_000 + i64::from(uid)],
                    )
                    .unwrap();
                db.conn()
                    .execute(
                        "INSERT INTO messages (
                            id, account_id, thread_id, folder_id, provider_uid,
                            message_id_header, in_reply_to, date, sender_name, sender_email,
                            subject, snippet, unread, starred, has_attachment, importance, size_bytes
                         ) VALUES (
                            ?1, 'john', ?2, 'inbox', ?3,
                            ?4, ?5, ?6, '', 'a@b.com',
                            ?7, '', 1, 0, 0, 0, 0
                         )",
                        params![
                            format!("msg:john:inbox:{uid}"),
                            format!("thr:john:inbox:{uid}"),
                            uid,
                            mid_s,
                            irt,
                            1_000 + i64::from(uid),
                            subj,
                        ],
                    )
                    .unwrap();
            }
        }
        let core = Core::open(&path).unwrap();
        let page = core
            .list_threads(crate::command::ListThreadsQuery {
                account_id: john(),
                folder_id: crate::model::FolderId("inbox".into()),
                filter: crate::model::ThreadFilter::All,
                after: None,
                limit: 64,
            })
            .unwrap();
        assert_eq!(
            page.total, 1,
            "Core::open must regroup already-stored 1:1 mail"
        );
        assert_eq!(page.threads[0].message_count, 3);
        let flag: String = core
            .db
            .conn()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![crate::threading::RETHREAD_SETTINGS_KEY],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(flag, "1");
    }

    /// Live 1:1 profile: T-028 already zeroed `threads.unread`, IMAP still
    /// has `messages.unread = 1`. First `Core::open` must copy flags down
    /// *before* whole-folder rollup, or grouping resurrects unread.
    /// Mutation: delete the `UPDATE messages SET unread = (SELECT t.unread…)`
    /// in `rethread_folder` — this test fails by name.
    #[test]
    fn core_open_copy_down_keeps_locally_read_mail_read_when_rethreading() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let db = Database::open(&path).unwrap();
            seed_account_and_folder(&db);
            for (uid, mid, irt, subj) in [
                (1_u32, "<a@x>", None, "Root"),
                (2, "<b@x>", Some("<a@x>"), "Re"),
                (3, "<c@x>", Some("<b@x>"), "Re"),
            ] {
                let tid = format!("thr:john:inbox:{uid}");
                db.conn()
                    .execute(
                        "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
                         VALUES (?1, 'john', 'inbox', ?2, '', ?3, 0, 0)",
                        params![tid, subj, 1_000 + i64::from(uid)],
                    )
                    .unwrap();
                db.conn()
                    .execute(
                        "INSERT INTO messages (
                            id, account_id, thread_id, folder_id, provider_uid,
                            message_id_header, in_reply_to, date, sender_name, sender_email,
                            subject, snippet, unread, starred, has_attachment, importance, size_bytes
                         ) VALUES (
                            ?1, 'john', ?2, 'inbox', ?3,
                            ?4, ?5, ?6, '', 'a@b.com',
                            ?7, '', 1, 0, 0, 0, 0
                         )",
                        params![
                            format!("msg:john:inbox:{uid}"),
                            format!("thr:john:inbox:{uid}"),
                            uid,
                            mid,
                            irt,
                            1_000 + i64::from(uid),
                            subj,
                        ],
                    )
                    .unwrap();
            }
        }
        let core = Core::open(&path).unwrap();
        let page = core
            .list_threads(crate::command::ListThreadsQuery {
                account_id: john(),
                folder_id: crate::model::FolderId("inbox".into()),
                filter: crate::model::ThreadFilter::All,
                after: None,
                limit: 64,
            })
            .unwrap();
        assert_eq!(page.total, 1, "1:1 A←B←C must become one thread");
        assert_eq!(page.threads[0].message_count, 3);
        assert!(
            !page.threads[0].unread(),
            "copy-down must keep locally-read mail read"
        );
        let max_msg: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT MAX(unread) FROM messages WHERE thread_id = ?1",
                params![page.threads[0].id.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            max_msg, 0,
            "messages.unread must be copied from threads before rollup"
        );
    }

    /// Two independent threads in one folder. MarkRead the first, then a
    /// flags-only upsert of the second UID and a separate empty slice.
    /// The first stays `threads.unread=0` **and** `messages.unread=0`.
    /// Mutation: restore whole-folder `WHERE folder_id` in rollup without
    /// an id filter — this test fails by name.
    #[test]
    fn mark_read_survives_flags_only_upsert_of_a_different_thread_and_empty_batch() {
        let mut core = Core::memory().unwrap();
        seed_account_and_folder(&core.db);
        let mut store = CoreSyncStore::new(&core.db, "john", "inbox");
        let a = header(1, "A", "a@b.com", "01 Jan 2024 10:00:00 +0000");
        let b = header(2, "B", "a@b.com", "01 Jan 2024 11:00:00 +0000");
        store.upsert_headers("INBOX", &[a, b]).unwrap();
        assert_eq!(distinct_thread_ids(&core.db), 2);
        let tid_a = thread_id_for_uid(&core.db, 1);
        drop(store);
        core.dispatch(crate::command::Command::MarkRead {
            account_id: john(),
            thread_ids: vec![crate::model::ThreadId(tid_a.clone())],
        })
        .unwrap();
        assert_eq!(unread_flags(&core.db, &tid_a), (0, 0));
        // Rollup rewrites message_count from members. A whole-folder
        // `WHERE folder_id` would reset this even when unread already
        // matches (apply_one wrote messages); scoped rollup must not.
        core.db
            .conn()
            .execute(
                "UPDATE threads SET message_count = 99 WHERE id = ?1",
                params![tid_a],
            )
            .unwrap();

        let mut store = CoreSyncStore::new(&core.db, "john", "inbox");
        let flags_b = HeaderMeta {
            uid: 2,
            flags: vec!["\\Seen".into()],
            ..HeaderMeta::default()
        };
        store
            .upsert_headers("INBOX", std::slice::from_ref(&flags_b))
            .unwrap();
        assert_eq!(
            unread_flags(&core.db, &tid_a),
            (0, 0),
            "flags-only of another UID must not roll up this thread"
        );
        store.upsert_headers("INBOX", &[]).unwrap();
        assert_eq!(
            unread_flags(&core.db, &tid_a),
            (0, 0),
            "empty upsert_headers must be a no-op, not a folder-wide rollup"
        );
        let count: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT message_count FROM threads WHERE id = ?1",
                params![tid_a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 99,
            "rollup must not rewrite an unrelated thread in the same folder"
        );
    }

    /// Reply (1:1) → MarkRead → parent in the next batch merges. Survivor
    /// stays read. Pins `apply_one` writing `messages.unread`: without it,
    /// rollup of the survivor from `MAX(m.unread)` restores unread.
    ///
    /// The parent is `\Seen` so it does not independently introduce an
    /// unread member — OR/MAX across members would then correctly keep
    /// the thread unread even with the messages write.
    #[test]
    fn mark_read_survives_parent_merging_into_the_read_reply() {
        let mut core = Core::memory().unwrap();
        seed_account_and_folder(&core.db);
        let mut store = CoreSyncStore::new(&core.db, "john", "inbox");
        let reply = HeaderMeta {
            message_id: Some("<b@x>".into()),
            in_reply_to: Some("<a@x>".into()),
            ..header(2, "Re: Root", "a@b.com", "01 Jan 2024 11:00:00 +0000")
        };
        store
            .upsert_headers("INBOX", std::slice::from_ref(&reply))
            .unwrap();
        let reply_tid = thread_id_for_uid(&core.db, 2);
        drop(store);
        core.dispatch(crate::command::Command::MarkRead {
            account_id: john(),
            thread_ids: vec![crate::model::ThreadId(reply_tid.clone())],
        })
        .unwrap();
        assert_eq!(unread_flags(&core.db, &reply_tid), (0, 0));

        let parent = HeaderMeta {
            message_id: Some("<a@x>".into()),
            flags: vec!["\\Seen".into()],
            ..header(1, "Root", "a@b.com", "01 Jan 2024 10:00:00 +0000")
        };
        let mut store = CoreSyncStore::new(&core.db, "john", "inbox");
        store
            .upsert_headers("INBOX", std::slice::from_ref(&parent))
            .unwrap();
        assert_eq!(distinct_thread_ids(&core.db), 1);
        let survivor = thread_id_for_uid(&core.db, 1);
        assert_eq!(
            unread_flags(&core.db, &survivor),
            (0, 0),
            "merged survivor must stay read"
        );
        let reply_unread: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT unread FROM messages WHERE provider_uid = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reply_unread, 0);
    }

    /// Flag already set + three 1:1 A←B←C rows on disk → `Core::open` must
    /// not glue them. The sibling `core_open_rethreads_existing_one_to_one_mail_once`
    /// only covers the first open and would miss dropping the early-return.
    #[test]
    fn core_open_skips_rethread_when_jwz_flag_is_already_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let db = Database::open(&path).unwrap();
            seed_account_and_folder(&db);
            for (uid, mid, irt, subj) in [
                (1_u32, "<a@x>", None, "Root"),
                (2, "<b@x>", Some("<a@x>"), "Re"),
                (3, "<c@x>", Some("<b@x>"), "Re"),
            ] {
                let tid = format!("thr:john:inbox:{uid}");
                db.conn()
                    .execute(
                        "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
                         VALUES (?1, 'john', 'inbox', ?2, '', ?3, 1, 0)",
                        params![tid, subj, 1_000 + i64::from(uid)],
                    )
                    .unwrap();
                db.conn()
                    .execute(
                        "INSERT INTO messages (
                            id, account_id, thread_id, folder_id, provider_uid,
                            message_id_header, in_reply_to, date, sender_name, sender_email,
                            subject, snippet, unread, starred, has_attachment, importance, size_bytes
                         ) VALUES (
                            ?1, 'john', ?2, 'inbox', ?3,
                            ?4, ?5, ?6, '', 'a@b.com',
                            ?7, '', 1, 0, 0, 0, 0
                         )",
                        params![
                            format!("msg:john:inbox:{uid}"),
                            format!("thr:john:inbox:{uid}"),
                            uid,
                            mid,
                            irt,
                            1_000 + i64::from(uid),
                            subj,
                        ],
                    )
                    .unwrap();
            }
            db.conn()
                .execute(
                    "INSERT INTO settings (key, value) VALUES (?1, '1')",
                    params![crate::threading::RETHREAD_SETTINGS_KEY],
                )
                .unwrap();
        }
        let core = Core::open(&path).unwrap();
        let page = core
            .list_threads(crate::command::ListThreadsQuery {
                account_id: john(),
                folder_id: crate::model::FolderId("inbox".into()),
                filter: crate::model::ThreadFilter::All,
                after: None,
                limit: 64,
            })
            .unwrap();
        assert_eq!(
            page.total, 3,
            "Core::open must not regroup when threading_jwz_v1 is already 1"
        );
    }

    /// Flags-only HeaderMeta carries `gm_thrid = None` (CONDSTORE). COALESCE
    /// must keep the value a full fetch already wrote.
    #[test]
    fn flags_only_upsert_does_not_clobber_gm_thrid() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let full = HeaderMeta {
            gm_thrid: Some("999".into()),
            ..header(
                7,
                "Original Subject",
                "Jane Doe <jane@example.com>",
                "01 Jan 2024 10:00:00 +0000",
            )
        };
        store
            .upsert_headers("INBOX", std::slice::from_ref(&full))
            .unwrap();
        let flags_only = HeaderMeta {
            uid: 7,
            flags: vec!["\\Seen".to_string()],
            ..HeaderMeta::default()
        };
        store
            .upsert_headers("INBOX", std::slice::from_ref(&flags_only))
            .unwrap();
        let gm: Option<String> = db
            .conn()
            .query_row(
                "SELECT gm_thrid FROM threads WHERE id = (SELECT thread_id FROM messages WHERE provider_uid = 7)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            gm.as_deref(),
            Some("999"),
            "flags-only upsert must COALESCE gm_thrid, not write NULL"
        );
    }

    /// Store path through `upsert_headers` / `load_hints`. Distinct
    /// `X-GM-THRID` must not merge even when In-Reply-To would link them.
    /// The refuse lives in `threading.rs` too; this pins that `load_hints`
    /// actually reads `t.gm_thrid`.
    #[test]
    fn distinct_gm_thrid_does_not_merge_through_upsert_even_when_in_reply_to_links() {
        let db = Database::memory().unwrap();
        seed_account_and_folder(&db);
        let mut store = CoreSyncStore::new(&db, "john", "inbox");
        let a = HeaderMeta {
            message_id: Some("<a@x>".into()),
            gm_thrid: Some("111".into()),
            ..header(1, "Root", "a@b.com", "01 Jan 2024 10:00:00 +0000")
        };
        let b = HeaderMeta {
            message_id: Some("<b@x>".into()),
            in_reply_to: Some("<a@x>".into()),
            gm_thrid: Some("222".into()),
            ..header(2, "Re: Root", "a@b.com", "01 Jan 2024 11:00:00 +0000")
        };
        store.upsert_headers("INBOX", &[a, b]).unwrap();
        assert_eq!(
            distinct_thread_ids(&db),
            2,
            "distinct gm_thrid must refuse JWZ merge through the store"
        );
        assert_eq!(thread_count(&db), 2);
    }
}
