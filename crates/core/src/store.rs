//! SQLite-backed command/query bus (T-007). No GTK, no IMAP.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

use feathermail_db::Database;
use feathermail_html::decode_encoded_words;
use feathermail_security::SecretStore;
use rusqlite::{params, OptionalExtension};

use crate::command::{Command, ListThreadsQuery, MailEvent, UnifiedThreadsQuery};
use crate::error::{CoreError, ErrorCode};
use crate::mailbox::{
    normalize_host, unique_account_id, AccountEdit, AddAccountError, MailSecurity, MailboxForm,
    MailboxFormError,
};
use crate::model::{
    Account, AccountId, AccountStatus, Address, Attachment, AttachmentDownload, AttachmentEncoding,
    AttachmentId, CreateFolderError, DeleteFolderError, Draft, DraftAttachment, DraftContent,
    DraftId, Folder, FolderId, FolderKind, FolderSummary, Importance, MessageId, OpKind,
    OperationId, OutboxMessage, OutgoingAttachment, Placement, RenameFolderError, ResponseKind,
    Thread, ThreadCursor, ThreadFilter, ThreadId, ThreadMessage, ThreadPage, LIST_PAGE,
};
use crate::provider::MailConnector;
use crate::settings::SettingsStore;
use crate::sync_store::CoreSyncStore;
use crate::threading::RETHREAD_SETTINGS_KEY;

/// T-044's per-file compose guard. The draft keeps a path and metadata, not
/// attachment bytes, so this protects both provider compatibility and local
/// resource use before the outbox is queued.
const MAX_OUTGOING_ATTACHMENT_BYTES: u64 = 100 * 1024 * 1024;

pub struct Core {
    pub(crate) db: Database,
    pub(crate) settings: SettingsStore,
    listeners: Vec<Sender<MailEvent>>,
    now: Option<i64>,
}

/// Operation ids produced by one [`Core::dispatch_with_receipt`] call.
///
/// The thread id is kept alongside the queue id so a caller handling a
/// multi-thread command can correlate each requested target without
/// reimplementing Core's deterministic id scheme.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DispatchReceipt {
    pub operations: Vec<OperationReceipt>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationReceipt {
    pub thread_id: ThreadId,
    pub operation_id: OperationId,
}

/// Durable handle handed to the UI for the ten-second Undo affordance. It
/// contains only an opaque operation id; all state and payloads remain in
/// Core/SQLite (D14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoTicket {
    pub operation_id: OperationId,
}

impl OperationReceipt {
    pub fn undo_ticket(&self) -> UndoTicket {
        UndoTicket {
            operation_id: self.operation_id.clone(),
        }
    }
}

/// Result of consuming an Undo ticket. A pending operation is cancelled in
/// place. Once it may already have reached the server, Core creates a causal
/// reverse operation and returns its id so callers can wake the worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoReceipt {
    Cancelled {
        operation_id: OperationId,
    },
    ReverseQueued {
        operation_id: OperationId,
        reverse_operation_id: OperationId,
    },
}

/// Best-effort outcome of the keyring half of [`Core::remove_account`].
/// Local SQLite removal has already committed by the time this comes back;
/// `keyring_error` is a human string (never the secret, never the account
/// id) for the caller to surface or log, and is `None` only when every
/// secret kind for the account was confirmed deleted (or never existed).
/// `Some` means *at least one* secret kind may still be sitting in the
/// system keyring — [`SecretStore::delete_account`] attempts every kind
/// even after an earlier one fails, so this is not "the keyring was
/// entirely unreachable," it can also mean "two of three came out
/// cleanly." The caller must not present `Some` as harmless.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoveAccountReport {
    pub keyring_error: Option<String>,
}

/// What [`Core::account_connection`] answers: how to reach one saved
/// account's servers, and which provider it was added as (`generic`,
/// `gmail`, `microsoft`) so the caller knows which connector and which
/// keyring key belong to it. Carries no secret (D14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountConnection {
    pub provider: String,
    pub form: MailboxForm,
}

/// Snapshot of an `accounts` row read for [`Core::update_account`].
struct AccountRow {
    provider: String,
    name: String,
    email: String,
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    imap_security: MailSecurity,
    smtp_security: MailSecurity,
}

/// SQL projection backing [`Core::attachment_download_target`]. Kept as one
/// alias because rusqlite maps tuples positionally; naming the row here makes
/// that unavoidable boundary auditable without a `type_complexity` escape.
type AttachmentDownloadRow = (
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
    String,
    String,
);

impl Core {
    pub fn open_default() -> Result<Self, CoreError> {
        Self::open(feathermail_db::default_db_path())
    }

    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, CoreError> {
        let db = Database::open(path).map_err(db_err)?;
        let settings = SettingsStore::load(&db)?;
        let core = Self {
            db,
            settings,
            listeners: Vec::new(),
            now: None,
        };
        core.recover_inflight()?;
        core.maybe_rethread_existing_mail()?;
        core.ensure_default_mcp_client_if_enabled()?;
        Ok(core)
    }

    pub fn memory() -> Result<Self, CoreError> {
        let db = Database::memory().map_err(db_err)?;
        let settings = SettingsStore::load(&db)?;
        let core = Self {
            db,
            settings,
            listeners: Vec::new(),
            now: None,
        };
        core.recover_inflight()?;
        core.maybe_rethread_existing_mail()?;
        core.ensure_default_mcp_client_if_enabled()?;
        Ok(core)
    }

    pub fn set_now(&mut self, now: i64) {
        self.now = Some(now);
    }

    /// T-035/D26: release every local snooze whose durable deadline has
    /// passed. Snooze is intentionally not represented by an IMAP command;
    /// this is the scheduler's Core-side wake door, used by the service
    /// worker before it considers provider connectivity. The Inbox folder is
    /// resolved from the account's real folder row, with the stable
    /// `account:inbox` id as a safe fallback for profiles not yet discovered
    /// by LIST.
    pub fn wake_due_snoozes(&mut self) -> Result<Vec<(AccountId, ThreadId)>, CoreError> {
        let now = self.now();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        let due: Vec<(String, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT s.account_id, s.thread_id
                     FROM snoozes s
                     JOIN threads t ON t.account_id = s.account_id AND t.id = s.thread_id
                     WHERE s.until_ts <= ?1 OR t.snooze_until <= ?1
                     ORDER BY s.until_ts, s.account_id, s.thread_id",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![now], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            rows
        };
        for (account_id, thread_id) in &due {
            wake_snooze_in(&tx, account_id, thread_id)?;
        }
        tx.commit().map_err(sql_err)?;
        let out: Vec<_> = due
            .into_iter()
            .map(|(account_id, thread_id)| (AccountId(account_id), ThreadId(thread_id)))
            .collect();
        for (account_id, thread_id) in &out {
            self.emit(MailEvent::ThreadsChanged {
                account_id: account_id.clone(),
                thread_ids: vec![thread_id.clone()],
            });
        }
        Ok(out)
    }

    /// T-061e: bring one snoozed thread back now, exactly as its own timer
    /// would have.
    ///
    /// This is the same local transition as [`Core::wake_due_snoozes`] --
    /// literally the same statements, via `wake_snooze_in` -- with the
    /// deadline test dropped. That sharing is the point: an unsnooze that
    /// restored a thread differently from the timer would leave two
    /// definitions of "no longer snoozed", and only one of them would be the
    /// one the user has already seen happen. Snooze is a local overlay
    /// (D26), so nothing is queued for IMAP here either; the thread returns
    /// to Inbox, the `local` snooze ledger row is cancelled, and the snooze
    /// itself is gone.
    ///
    /// Returns `false` when the thread is not snoozed, so a caller can tell
    /// "there was nothing to undo" from "done". An unknown account is
    /// `ErrorCode::AccountNotFound`; an unknown thread in a known account is
    /// simply not snoozed.
    pub fn unsnooze_thread(
        &mut self,
        account_id: &AccountId,
        thread_id: &ThreadId,
    ) -> Result<bool, CoreError> {
        self.require_account(account_id.as_str())?;
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        let snoozed: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM snoozes WHERE account_id = ?1 AND thread_id = ?2",
                params![account_id.as_str(), thread_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        if snoozed == 0 {
            return Ok(false);
        }
        wake_snooze_in(&tx, account_id.as_str(), thread_id.as_str())?;
        tx.commit().map_err(sql_err)?;
        self.emit(MailEvent::ThreadsChanged {
            account_id: account_id.clone(),
            thread_ids: vec![thread_id.clone()],
        });
        Ok(true)
    }

    /// T-035: the next local deadline, optionally restricted to one account.
    /// Returning the delay source lets the worker sleep until a snooze wakes
    /// instead of relying on its general idle poll.
    pub fn next_snooze_deadline(
        &self,
        account_id: Option<&AccountId>,
    ) -> Result<Option<i64>, CoreError> {
        let conn = self.db.conn();
        let deadline = match account_id {
            Some(account_id) => conn
                .query_row(
                    "SELECT MIN(s.until_ts)
                     FROM snoozes s
                     WHERE s.account_id = ?1",
                    params![account_id.as_str()],
                    |row| row.get(0),
                )
                .map_err(sql_err)?,
            None => conn
                .query_row("SELECT MIN(until_ts) FROM snoozes", [], |row| row.get(0))
                .map_err(sql_err)?,
        };
        Ok(deadline)
    }

    pub fn subscribe(&mut self) -> Receiver<MailEvent> {
        let (tx, rx) = mpsc::channel();
        self.listeners.push(tx);
        rx
    }

    pub fn dispatch(&mut self, cmd: Command) -> Result<(), CoreError> {
        self.dispatch_with_receipt(cmd).map(|_| ())
    }

    /// Dispatch a mutation and return the exact queue ids created or
    /// deduplicated for its thread targets. The legacy [`Self::dispatch`]
    /// remains the compatibility wrapper for callers that do not need the
    /// receipt.
    pub fn dispatch_with_receipt(&mut self, cmd: Command) -> Result<DispatchReceipt, CoreError> {
        let account_id = cmd.account_id().clone();
        self.require_account(account_id.as_str())?;
        let now = self.now();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        let receipt = dispatch_with_receipt_in(&tx, &cmd, now)?;
        tx.commit().map_err(sql_err)?;
        self.emit(MailEvent::ThreadsChanged {
            account_id,
            thread_ids: cmd.thread_ids(),
        });
        Ok(receipt)
    }

    /// T-106: mark every unread thread in one folder read, in the single
    /// transaction [`Self::dispatch_with_receipt`] already gives a list of
    /// threads.
    ///
    /// The folder predicate is [`folder_filter`], the same one
    /// [`Self::list_threads`] pages with, so "all" means exactly the
    /// threads that folder shows -- including the virtual ones (Starred,
    /// Snoozed, Archive, Trash), where "the folder's mail" is a condition
    /// rather than a `folder_id`. Nothing unread means no work and no
    /// operations: an empty receipt, not [`ErrorCode::InvalidArgument`],
    /// because a folder somebody else just finished reading is not a
    /// caller error.
    pub fn mark_folder_read(
        &mut self,
        account_id: &AccountId,
        folder_id: &FolderId,
    ) -> Result<DispatchReceipt, CoreError> {
        self.require_account(account_id.as_str())?;
        let thread_ids = {
            let conn = self.db.conn();
            let (rest, bind_folder) = folder_filter(folder_id.as_str());
            let sql = format!(
                "SELECT t.id FROM threads t WHERE t.account_id = ? AND {rest} AND t.unread = 1 \
                 ORDER BY t.date DESC, t.id DESC"
            );
            let mut binds: Vec<Box<dyn rusqlite::ToSql>> =
                vec![Box::new(account_id.as_str().to_string())];
            if bind_folder {
                binds.push(Box::new(folder_id.as_str().to_string()));
            }
            let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
            let p: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
            let ids = stmt
                .query_map(p.as_slice(), |row| row.get::<_, String>(0))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            ids.into_iter().map(ThreadId).collect::<Vec<_>>()
        };
        if thread_ids.is_empty() {
            return Ok(DispatchReceipt {
                operations: Vec::new(),
            });
        }
        self.dispatch_with_receipt(Command::MarkRead {
            account_id: account_id.clone(),
            thread_ids,
        })
    }

    /// Consume a durable Undo ticket. The operation row is never deleted:
    /// this is what makes a click racing a provider apply safe after a
    /// restart. Pending work is cancelled atomically with its optimistic
    /// rollback; running/acked work gets a causal reverse operation.
    pub fn undo(&mut self, ticket: &UndoTicket) -> Result<UndoReceipt, CoreError> {
        let requested_at = self.now();
        let tx = self.db.immediate_transaction().map_err(sql_err)?;
        let original: UndoOperation = tx
            .query_row(
                "SELECT id, account_id, target_id, op, status, undo_payload
                 FROM operations WHERE id = ?1",
                params![ticket.operation_id.as_str()],
                UndoOperation::from_row,
            )
            .optional()
            .map_err(sql_err)?
            .ok_or_else(|| CoreError::from_code(ErrorCode::OperationNotSupported))?;
        let (receipt, event) = consume_undo_in(&tx, &original, requested_at)?;
        tx.commit().map_err(sql_err)?;
        self.emit(event);
        Ok(receipt)
    }

    /// Restore a thread only through its exact durable Trash lifecycle.
    ///
    /// Unlike the GTK toast, callers do not hold an [`UndoTicket`]. This
    /// narrow door finds one reversible Trash operation only after proving
    /// that the thread is still in the local Trash state (the overlay or a
    /// discovered real Trash folder) and that a newer
    /// placement-changing intent cannot make that old snapshot stale. The
    /// candidate selection and cancellation/reverse share one immediate
    /// SQLite transaction, so a competing placement change cannot make its
    /// snapshot stale. Pending work rolls back and sent/running work gets the
    /// same causal provider Move as the UI lifecycle.
    pub fn restore_trashed_thread(
        &mut self,
        account_id: &AccountId,
        thread_id: &ThreadId,
    ) -> Result<UndoReceipt, CoreError> {
        self.require_account(account_id.as_str())?;
        let requested_at = self.now();
        let tx = self.db.immediate_transaction().map_err(sql_err)?;
        let current_trash: Option<(i64, i64)> = tx
            .query_row(
                "SELECT t.deleted,
                        EXISTS (
                            SELECT 1 FROM folders f
                            WHERE f.id = t.folder_id
                              AND f.account_id = t.account_id
                              AND f.kind = 'trash'
                              AND f.remote_id IS NOT NULL
                              AND f.remote_id <> ''
                        )
                 FROM threads t WHERE t.account_id = ?1 AND t.id = ?2",
                params![account_id.as_str(), thread_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        match current_trash {
            None => return Err(CoreError::from_code(ErrorCode::MessageNotFound)),
            Some((0, 0)) => return Err(CoreError::from_code(ErrorCode::OperationNotSupported)),
            Some(_) => {}
        }
        let original: Option<UndoOperation> = tx
            .query_row(
                "SELECT original.id, original.account_id, original.target_id,
                        original.op, original.status, original.undo_payload
                 FROM operations AS original
                 WHERE original.account_id = ?1
                   AND original.target_id = ?2
                   AND original.op = 'trash'
                   AND original.status IN ('pending', 'running', 'acked')
                   AND original.undo_requested_at IS NULL
                   AND NOT EXISTS (
                       SELECT 1
                       FROM operations AS later
                       WHERE later.account_id = original.account_id
                         AND later.target_id = original.target_id
                         AND later.rowid > original.rowid
                         AND later.op IN ('trash', 'permanent_delete', 'archive', 'move', 'snooze')
                         AND later.status IN ('pending', 'running', 'acked', 'local')
                   )
                ORDER BY original.rowid DESC
                LIMIT 1",
                params![account_id.as_str(), thread_id.as_str()],
                UndoOperation::from_row,
            )
            .optional()
            .map_err(sql_err)?;
        let Some(original) = original else {
            return Err(CoreError::from_code(ErrorCode::OperationNotSupported));
        };
        let (receipt, event) = consume_undo_in(&tx, &original, requested_at)?;
        tx.commit().map_err(sql_err)?;
        self.emit(event);
        Ok(receipt)
    }

    pub fn list_threads(&self, q: ListThreadsQuery) -> Result<ThreadPage, CoreError> {
        self.require_account(q.account_id.as_str())?;
        let limit = if q.limit == 0 { LIST_PAGE } else { q.limit };
        let conn = self.db.conn();
        let (rest, bind_folder) = folder_filter(q.folder_id.as_str());
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(q.account_id.as_str().to_string())];
        if bind_folder {
            binds.push(Box::new(q.folder_id.as_str().to_string()));
        }
        // `ThreadFilter` is a closed enum, so this static SQL fragment is
        // not built from user input. Keeping it in the Core query (rather
        // than dropping non-matches after GTK receives a page) makes both
        // pagination and `ThreadPage::total` describe exactly what the
        // person selected in the Filter popover.
        let filter = thread_filter_sql(q.filter);
        let where_sql = format!("t.account_id = ? AND {rest} AND {filter}");
        // D15 is a production contract for a real folder page, not merely
        // the standalone db query-plan test. A broad account/date index is
        // useful to FTS search, but would make this path scan other folders
        // just to preserve ordering; pin the folder-local index whenever
        // `folder_filter` actually bound a concrete folder id.
        let page_source = thread_page_source(bind_folder);
        let total: usize = {
            let sql = format!("SELECT COUNT(*) FROM {page_source} WHERE {where_sql}");
            let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
            let p: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
            stmt.query_row(p.as_slice(), |row| row.get::<_, i64>(0))
                .map_err(sql_err)? as usize
        };
        let mut page_sql = format!(
            "SELECT {THREAD_COLUMNS} FROM {page_source} {THREAD_LATEST_JOIN} WHERE {where_sql}"
        );
        if let Some(cur) = &q.after {
            page_sql.push_str(" AND (t.date < ? OR (t.date = ? AND t.id < ?))");
            binds.push(Box::new(cur.date));
            binds.push(Box::new(cur.date));
            binds.push(Box::new(cur.id.as_str().to_string()));
        }
        page_sql.push_str(" ORDER BY t.date DESC, t.id DESC LIMIT ?");
        binds.push(Box::new(limit as i64 + 1));
        let mut stmt = conn.prepare(&page_sql).map_err(sql_err)?;
        let p: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let mut threads = stmt
            .query_map(p.as_slice(), map_thread)
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        let next = if threads.len() > limit {
            threads.truncate(limit);
            threads.last().map(ThreadCursor::of)
        } else {
            None
        };
        Ok(ThreadPage {
            threads,
            next,
            prev: None,
            total,
        })
    }

    /// T-108: one page of the unified mailbox -- the same folder read
    /// across every account at once, newest first.
    ///
    /// Thread ids carry their account (`thr:<account>:...`), so the
    /// existing `(date, id)` cursor stays a total order across the merge
    /// and pagination needs nothing new. What the page cannot do is answer
    /// "which account is this" for the caller: it does not have to, because
    /// every [`Thread`] it returns carries its own `account_id` -- that is
    /// what the shell aims an action at.
    pub fn list_unified_threads(&self, q: UnifiedThreadsQuery) -> Result<ThreadPage, CoreError> {
        let Some(rest) = unified_folder_filter(q.kind) else {
            return Err(CoreError::from_code(ErrorCode::InvalidArgument));
        };
        let limit = if q.limit == 0 { LIST_PAGE } else { q.limit };
        let conn = self.db.conn();
        let filter = thread_filter_sql(q.filter);
        let where_sql = format!("{rest} AND {filter}");
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let total: usize = {
            let sql = format!("SELECT COUNT(*) FROM threads t WHERE {where_sql}");
            conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
                .map_err(sql_err)? as usize
        };
        let mut page_sql = format!(
            "SELECT {THREAD_COLUMNS} FROM threads t {THREAD_LATEST_JOIN} WHERE {where_sql}"
        );
        if let Some(cur) = &q.after {
            page_sql.push_str(" AND (t.date < ? OR (t.date = ? AND t.id < ?))");
            binds.push(Box::new(cur.date));
            binds.push(Box::new(cur.date));
            binds.push(Box::new(cur.id.as_str().to_string()));
        }
        page_sql.push_str(" ORDER BY t.date DESC, t.id DESC LIMIT ?");
        binds.push(Box::new(limit as i64 + 1));
        let mut stmt = conn.prepare(&page_sql).map_err(sql_err)?;
        let p: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let mut threads = stmt
            .query_map(p.as_slice(), map_thread)
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        let next = if threads.len() > limit {
            threads.truncate(limit);
            threads.last().map(ThreadCursor::of)
        } else {
            None
        };
        Ok(ThreadPage {
            threads,
            next,
            prev: None,
            total,
        })
    }

    /// T-108: the unified mailbox's sidebar -- four rows, counts summed
    /// over every account. Ids are `unified:<kind>`, which no real folder
    /// can collide with (a real id is `<account>:<...>` and an account id
    /// never contains a colon), so the shell can tell one from the other by
    /// the id alone.
    pub fn list_unified_folders(&self) -> Result<Vec<FolderSummary>, CoreError> {
        let conn = self.db.conn();
        let mut out = Vec::with_capacity(FolderKind::UNIFIED_ORDER.len());
        for kind in FolderKind::UNIFIED_ORDER {
            let Some(rest) = unified_folder_filter(kind) else {
                continue;
            };
            let sql =
                format!("SELECT COUNT(*), COALESCE(SUM(t.unread), 0) FROM threads t WHERE {rest}");
            let (total, unread): (i64, i64) = conn
                .query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(sql_err)?;
            out.push(FolderSummary {
                folder: Folder {
                    id: FolderId(format!("unified:{}", kind.as_str())),
                    label: kind.default_label().to_string(),
                    kind,
                    color: None,
                    account_id: None,
                    create_failed: false,
                },
                unread: nonneg(unread),
                total: nonneg(total),
            });
        }
        Ok(out)
    }

    /// T-108: mark one unified folder read -- every account's copy of it,
    /// in one call, so the sidebar item behaves the same in the merged view
    /// as it does in a single mailbox (T-106). The per-account door is
    /// [`Self::mark_folder_read`]; this only decides which folders it is
    /// pointed at, and merges the receipts so one toast still says how many
    /// threads in total.
    pub fn mark_unified_folder_read(
        &mut self,
        kind: FolderKind,
    ) -> Result<DispatchReceipt, CoreError> {
        if unified_folder_filter(kind).is_none() {
            return Err(CoreError::from_code(ErrorCode::InvalidArgument));
        }
        let accounts: Vec<AccountId> = self
            .list_accounts()?
            .into_iter()
            .map(|account| account.id)
            .collect();
        let mut operations = Vec::new();
        for account_id in accounts {
            let folder_id = match kind {
                FolderKind::Starred | FolderKind::Trash => FolderId(kind.as_str().to_string()),
                _ => {
                    let Some(summary) = self
                        .list_folders(&account_id)?
                        .into_iter()
                        .find(|summary| summary.folder.kind == kind)
                    else {
                        continue;
                    };
                    summary.folder.id
                }
            };
            operations.extend(self.mark_folder_read(&account_id, &folder_id)?.operations);
        }
        Ok(DispatchReceipt { operations })
    }

    /// T-115: which mailbox a thread belongs to, by id alone.
    ///
    /// Thread ids are globally unique (`threads.id` is the primary key), so
    /// the merged view can ask this when it has a row id and no account in
    /// hand -- the list widget may not have the row yet, and guessing from
    /// the account menu would open the letter against the wrong session.
    pub fn account_id_for_thread(&self, id: &ThreadId) -> Result<AccountId, CoreError> {
        self.db
            .conn()
            .query_row(
                "SELECT account_id FROM threads WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0).map(AccountId),
            )
            .optional()
            .map_err(sql_err)?
            .ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))
    }

    /// T-024: opening a single thread uses the same latest-message projection
    /// as the page and search paths. That gives body-open callers the newest
    /// message's id and sender metadata without a second query shape.
    pub fn get_thread(&self, account_id: &AccountId, id: &ThreadId) -> Result<Thread, CoreError> {
        self.require_account(account_id.as_str())?;
        let conn = self.db.conn();
        let sql = format!(
            "SELECT {THREAD_COLUMNS} FROM threads t {THREAD_LATEST_JOIN} \
             WHERE t.account_id = ?1 AND t.id = ?2"
        );
        conn.query_row(
            &sql,
            params![account_id.as_str(), id.as_str()],
            map_thread_with_message_id,
        )
        .optional()
        .map_err(sql_err)?
        .ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))
    }

    /// T-029: messages of one conversation, oldest first, metadata only.
    /// Bodies stay behind [`Self::lookup_body`]. `get_thread` remains the
    /// list card (latest `message_id`); this is the opened-thread list.
    pub fn list_thread_messages(
        &self,
        account_id: &AccountId,
        thread_id: &ThreadId,
    ) -> Result<Vec<ThreadMessage>, CoreError> {
        self.require_account(account_id.as_str())?;
        let conn = self.db.conn();
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM threads WHERE account_id = ?1 AND id = ?2",
                params![account_id.as_str(), thread_id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(sql_err)?
            .is_some();
        if !exists {
            return Err(CoreError::from_code(ErrorCode::MessageNotFound));
        }
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {THREAD_MESSAGE_COLUMNS} FROM messages m
                 WHERE m.account_id = ?1 AND m.thread_id = ?2
                 ORDER BY m.date ASC, m.id ASC"
            ))
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(
                params![account_id.as_str(), thread_id.as_str()],
                map_thread_message,
            )
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(rows)
    }

    /// One account-scoped message metadata projection for callers that already
    /// hold a stable message id. It deliberately uses the opened-thread shape
    /// rather than body/cache lookup: no body or cache path crosses this Core
    /// door. The MCP boundary applies its own narrower public projection.
    pub fn get_thread_message(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
    ) -> Result<ThreadMessage, CoreError> {
        self.require_account(account_id.as_str())?;
        let sql = format!(
            "SELECT {THREAD_MESSAGE_COLUMNS} FROM messages m
             WHERE m.account_id = ?1 AND m.id = ?2"
        );
        self.db
            .conn()
            .query_row(
                &sql,
                params![account_id.as_str(), message_id.as_str()],
                map_thread_message,
            )
            .optional()
            .map_err(sql_err)?
            .ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))
    }

    /// One-shot regroup of already-downloaded 1:1 mail (T-029). Flag in
    /// `settings`, not a schema bump — otherwise live profiles stay 1:1
    /// forever after this lands.
    fn maybe_rethread_existing_mail(&self) -> Result<(), CoreError> {
        let conn = self.db.conn();
        let done: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![RETHREAD_SETTINGS_KEY],
                |r| r.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if done.as_deref() == Some("1") {
            return Ok(());
        }
        let folders: Vec<(String, String)> = {
            let mut stmt = conn
                .prepare("SELECT account_id, id FROM folders")
                .map_err(sql_err)?;
            let out = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            out
        };
        for (account_id, folder_id) in folders {
            CoreSyncStore::new(&self.db, account_id, folder_id)
                .rethread_folder()
                .map_err(|e| {
                    CoreError::new(ErrorCode::Conflict, "Couldn't regroup existing mail.")
                        .with_details(e.to_string())
                })?;
        }
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![RETHREAD_SETTINGS_KEY],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    /// T-074: every account on this computer, oldest first. Empty on a
    /// fresh profile — that is a valid answer, not an error; the shell
    /// routes an empty list to the Welcome screen itself.
    pub fn list_accounts(&self) -> Result<Vec<Account>, CoreError> {
        let conn = self.db.conn();
        let mut stmt = conn
            .prepare("SELECT id, name, email, status FROM accounts ORDER BY created_at ASC, id ASC")
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], map_account)
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(rows)
    }

    /// T-078: the connection settings saved for one account, in the same
    /// shape the "Add mailbox" wizard produced them.
    ///
    /// This exists because the thing that has to open a live IMAP session
    /// for a *saved* account -- the background sync worker in
    /// `feathermail-service` -- is outside this crate, and until now there
    /// was no way out: the row is read by the private `load_account_row`
    /// for [`Core::update_account`]'s own use, and `AccountRow` is private
    /// too. [`Core::list_accounts`] deliberately answers a different
    /// question (what the sidebar shows) and carries no host or port.
    ///
    /// D14: no secret crosses this boundary. The password or OAuth token
    /// stays in the keyring; [`AccountConnection::provider`] is what says
    /// which key to look under and which connector to build.
    pub fn account_connection(&self, id: &AccountId) -> Result<AccountConnection, CoreError> {
        let row = self.load_account_row(id.as_str())?;
        Ok(AccountConnection {
            provider: row.provider,
            form: MailboxForm {
                email: row.email,
                imap_host: row.imap_host,
                imap_port: row.imap_port,
                imap_security: row.imap_security,
                smtp_host: row.smtp_host,
                smtp_port: row.smtp_port,
                smtp_security: row.smtp_security,
            },
        })
    }

    /// Creates or updates one durable local draft. The caller supplies only
    /// editable fields; Core owns identity and timestamps so GTK, shortcuts,
    /// and MCP all use the same autosave contract (T-041).
    pub fn save_draft(
        &self,
        account_id: &AccountId,
        draft_id: Option<&DraftId>,
        content: DraftContent,
    ) -> Result<Draft, CoreError> {
        self.require_account(account_id.as_str())?;
        if content.from.trim().is_empty() {
            return Err(CoreError::new(
                ErrorCode::InvalidArgument,
                "Choose a From account before saving this draft.",
            ));
        }
        let now = self.now();
        // IMMEDIATE, not the default DEFERRED: everything below is a read
        // (sequence, owner) followed by a write. Under WAL a DEFERRED
        // transaction that read first gets SQLITE_BUSY_SNAPSHOT the instant
        // another handle commits in between -- returned immediately, without
        // consulting `busy_timeout` -- and autosave would fail for no reason
        // the user could act on. Taking the write lock up front also makes
        // the sequence lookup and the INSERT one linearization point.
        let tx = self.db.immediate_transaction_ref().map_err(sql_err)?;
        // No `unwrap_or(1)` here. A failed sequence query used to fall back
        // to id `draft:{account}:1`, which the upsert below then *overwrote*
        // -- the user's first draft replaced by whatever they were typing,
        // with `save_draft` still returning Ok. A sequence Core cannot
        // compute is an error, not a licence to reuse someone else's id.
        let id = match draft_id {
            Some(id) => id.clone(),
            None => DraftId(format!(
                "draft:{}:{}",
                account_id.as_str(),
                next_draft_sequence(&tx, account_id.as_str()).map_err(sql_err)?
            )),
        };
        let is_new = draft_id.is_none();
        if let Some(owner) = tx
            .query_row(
                "SELECT account_id FROM drafts WHERE id = ?1",
                params![id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql_err)?
        {
            if owner != account_id.as_str() {
                return Err(CoreError::from_code(ErrorCode::PermissionDenied));
            }
        }
        // Second line of defence for the same data loss: when the caller
        // asked for a *new* draft, a colliding id must raise UNIQUE rather
        // than quietly rewrite the row that already owns it. The upsert stays
        // for the autosave path, where updating the named draft is the point.
        const INSERT_NEW: &str = "INSERT INTO drafts
                 (id, account_id, thread_id, in_reply_to, from_addr, to_addr,
                  cc, bcc, subject, body, updated_at, remote_uid, sync_revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, 1)";
        const UPSERT: &str = "INSERT INTO drafts
                 (id, account_id, thread_id, in_reply_to, from_addr, to_addr,
                  cc, bcc, subject, body, updated_at, remote_uid, sync_revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, 1)
                 ON CONFLICT(id) DO UPDATE SET
                   thread_id=excluded.thread_id,
                   in_reply_to=excluded.in_reply_to,
                   from_addr=excluded.from_addr,
                   to_addr=excluded.to_addr,
                   cc=excluded.cc,
                   bcc=excluded.bcc,
                   subject=excluded.subject,
                   body=excluded.body,
                   updated_at=excluded.updated_at,
                   sync_revision=drafts.sync_revision + 1";
        tx.execute(
            if is_new { INSERT_NEW } else { UPSERT },
            params![
                id.as_str(),
                account_id.as_str(),
                content.thread_id.as_ref().map(ThreadId::as_str),
                content.in_reply_to.as_ref().map(MessageId::as_str),
                content.from.trim(),
                content.to.trim(),
                content.cc.trim(),
                content.bcc.trim(),
                content.subject,
                content.body,
                now,
            ],
        )
        .map_err(sql_err)?;
        let revision: i64 = tx
            .query_row(
                "SELECT sync_revision FROM drafts WHERE account_id = ?1 AND id = ?2",
                params![account_id.as_str(), id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        // Only the newest unsent revision may remain eligible. A worker
        // that already claimed an older row checks this revision before
        // touching IMAP, so it ACKs as stale instead of duplicating the
        // newest draft on the server.
        tx.execute(
            "UPDATE operations SET status = 'cancelled', next_attempt_at = NULL
             WHERE account_id = ?1 AND target_id = ?2 AND op = 'sync_draft'
               AND status IN ('pending', 'failed')",
            params![account_id.as_str(), id.as_str()],
        )
        .map_err(sql_err)?;
        enqueue(
            &tx,
            account_id.as_str(),
            id.as_str(),
            OpKind::SyncDraft,
            &revision.to_string(),
            None,
            now,
        )?;
        tx.commit().map_err(sql_err)?;
        self.get_draft(account_id, &id)
    }

    pub fn get_draft(
        &self,
        account_id: &AccountId,
        draft_id: &DraftId,
    ) -> Result<Draft, CoreError> {
        self.require_account(account_id.as_str())?;
        self.db
            .conn()
            .query_row(
                "SELECT id, account_id, thread_id, in_reply_to, from_addr,
                        to_addr, cc, bcc, subject, body, updated_at, remote_uid
                 FROM drafts WHERE account_id = ?1 AND id = ?2",
                params![account_id.as_str(), draft_id.as_str()],
                map_draft,
            )
            .optional()
            .map_err(sql_err)?
            .ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))
    }

    pub fn list_drafts(&self, account_id: &AccountId) -> Result<Vec<Draft>, CoreError> {
        self.require_account(account_id.as_str())?;
        let mut stmt = self
            .db
            .conn()
            .prepare(
                "SELECT id, account_id, thread_id, in_reply_to, from_addr,
                        to_addr, cc, bcc, subject, body, updated_at, remote_uid
                 FROM drafts d
                 WHERE account_id = ?1
                   AND NOT EXISTS (SELECT 1 FROM outbox o WHERE o.draft_id = d.id)
                 ORDER BY updated_at DESC, id DESC",
            )
            .map_err(sql_err)?;
        let drafts = stmt
            .query_map(params![account_id.as_str()], map_draft)
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(drafts)
    }

    pub fn latest_draft(&self, account_id: &AccountId) -> Result<Option<Draft>, CoreError> {
        Ok(self.list_drafts(account_id)?.into_iter().next())
    }

    /// Creates one editable response draft from synchronized metadata. This
    /// is intentionally a Core command rather than GTK/MCP string assembly:
    /// it owns the account boundary, Reply-all de-duplication, and the
    /// `in_reply_to` locator used later to freeze RFC threading headers.
    /// `quoted_body` is a local, already-prepared plain-text view supplied
    /// by the caller; Core never fetches or logs a mail body on this path.
    pub fn create_response_draft(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
        kind: ResponseKind,
        quoted_body: String,
    ) -> Result<Draft, CoreError> {
        self.require_account(account_id.as_str())?;
        let account_email: String = self
            .db
            .conn()
            .query_row(
                "SELECT email FROM accounts WHERE id = ?1",
                params![account_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        let source: (String, String, String, String, String) = self
            .db
            .conn()
            .query_row(
                "SELECT thread_id, sender_email, recipients, cc, subject
                 FROM messages WHERE account_id = ?1 AND id = ?2",
                params![account_id.as_str(), message_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_err)?
            .ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))?;
        let (thread_id, sender_email, recipients, original_cc, subject) = source;
        let sender_email = sender_email.trim();
        if !matches!(kind, ResponseKind::Forward) && !sender_email.contains('@') {
            return Err(CoreError::new(
                ErrorCode::InvalidArgument,
                "That message has no reply address.",
            ));
        }
        let (thread_id, in_reply_to, to, cc, subject) = match kind {
            ResponseKind::Reply => (
                Some(ThreadId(thread_id)),
                Some(message_id.clone()),
                sender_email.to_string(),
                String::new(),
                response_subject("Re:", &subject),
            ),
            ResponseKind::ReplyAll => (
                Some(ThreadId(thread_id)),
                Some(message_id.clone()),
                sender_email.to_string(),
                reply_all_cc(&recipients, &original_cc, &account_email, sender_email),
                response_subject("Re:", &subject),
            ),
            ResponseKind::Forward => (
                None,
                None,
                String::new(),
                String::new(),
                response_subject("Fwd:", &subject),
            ),
        };
        self.save_draft(
            account_id,
            None,
            DraftContent {
                thread_id,
                in_reply_to,
                from: account_email,
                to,
                cc,
                bcc: String::new(),
                subject,
                body: quoted_body,
            },
        )
    }

    /// Returns a draft only when the queued T-042 revision is still the
    /// current local revision. The queue stores the revision, never the
    /// draft's content, so the worker reads the body only here on its
    /// background thread (D11, D14).
    pub fn draft_for_sync(
        &self,
        account_id: &AccountId,
        draft_id: &DraftId,
        revision: i64,
    ) -> Result<Option<Draft>, CoreError> {
        self.require_account(account_id.as_str())?;
        self.db
            .conn()
            .query_row(
                "SELECT id, account_id, thread_id, in_reply_to, from_addr,
                        to_addr, cc, bcc, subject, body, updated_at, remote_uid
                 FROM drafts
                 WHERE account_id = ?1 AND id = ?2 AND sync_revision = ?3",
                params![account_id.as_str(), draft_id.as_str(), revision],
                map_draft,
            )
            .optional()
            .map_err(sql_err)
    }

    /// Records the UID returned by a successful draft APPEND, but only if
    /// the user has not edited the draft since that operation was claimed.
    /// A stale worker must never overwrite the newest server locator.
    pub fn mark_draft_remote_synced(
        &self,
        account_id: &AccountId,
        draft_id: &DraftId,
        revision: i64,
        remote_uid: Option<u32>,
    ) -> Result<bool, CoreError> {
        self.require_account(account_id.as_str())?;
        let changed = self
            .db
            .conn()
            .execute(
                "UPDATE drafts SET remote_uid = ?1
                 WHERE account_id = ?2 AND id = ?3 AND sync_revision = ?4",
                params![
                    remote_uid.map(i64::from),
                    account_id.as_str(),
                    draft_id.as_str(),
                    revision
                ],
            )
            .map_err(sql_err)?;
        Ok(changed != 0)
    }

    /// Local-only recipient typeahead from already synchronized mail. No
    /// contacts service and no network call are involved (T-040).
    pub fn suggest_addresses(
        &self,
        account_id: &AccountId,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<Address>, CoreError> {
        self.require_account(account_id.as_str())?;
        let needle = prefix.trim().to_ascii_lowercase();
        if needle.len() < 2 || limit == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .db
            .conn()
            .prepare(
                "SELECT sender_name, sender_email, recipients FROM (
                     SELECT sender_name, sender_email, recipients, date AS sort_date
                     FROM messages WHERE account_id = ?1
                     UNION ALL
                     SELECT '', '', trim(to_addr || ',' || cc || ',' || bcc),
                            created_at AS sort_date
                     FROM outbox
                     WHERE account_id = ?1 AND status IN ('delivered', 'sent')
                 )
                 ORDER BY sort_date DESC LIMIT 500",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![account_id.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(sql_err)?;
        let mut seen = std::collections::HashSet::new();
        let mut suggestions = Vec::new();
        for row in rows {
            let (name, email, recipients) = row.map_err(sql_err)?;
            push_address_suggestion(&mut suggestions, &mut seen, &needle, &name, &email, limit);
            for raw in recipients.split([',', ';']) {
                let email = raw
                    .trim()
                    .trim_matches(['<', '>', '"'])
                    .split_whitespace()
                    .last()
                    .unwrap_or_default()
                    .trim_matches(['<', '>']);
                push_address_suggestion(&mut suggestions, &mut seen, &needle, "", email, limit);
            }
            if suggestions.len() >= limit {
                break;
            }
        }
        Ok(suggestions)
    }

    pub fn delete_draft(
        &self,
        account_id: &AccountId,
        draft_id: &DraftId,
    ) -> Result<bool, CoreError> {
        self.require_account(account_id.as_str())?;
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        let changed = tx
            .execute(
                "DELETE FROM drafts WHERE account_id = ?1 AND id = ?2",
                params![account_id.as_str(), draft_id.as_str()],
            )
            .map_err(sql_err)?;
        if changed != 0 {
            tx.execute(
                "UPDATE operations SET status = 'cancelled', next_attempt_at = NULL
                 WHERE account_id = ?1 AND target_id = ?2 AND op = 'sync_draft'
                   AND status IN ('pending', 'failed')",
                params![account_id.as_str(), draft_id.as_str()],
            )
            .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;
        Ok(changed != 0)
    }

    /// Records only filesystem metadata and the source path for a draft
    /// attachment. In particular, this must not read the file: the source
    /// remains on disk until T-045 streams it to SMTP.
    pub fn attach_to_draft(
        &self,
        account_id: &AccountId,
        draft_id: &DraftId,
        path: &Path,
    ) -> Result<DraftAttachment, CoreError> {
        self.get_draft(account_id, draft_id)?;
        let metadata = std::fs::metadata(path).map_err(|_| {
            CoreError::new(
                ErrorCode::InvalidArgument,
                "That attachment is no longer available.",
            )
        })?;
        if !metadata.is_file() {
            return Err(CoreError::new(
                ErrorCode::InvalidArgument,
                "Choose a file to attach.",
            ));
        }
        if metadata.len() > MAX_OUTGOING_ATTACHMENT_BYTES {
            return Err(CoreError::new(
                ErrorCode::AttachmentTooLarge,
                "Attachments must be 100 MB or smaller.",
            ));
        }
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or_else(|| CoreError::from_code(ErrorCode::InvalidArgument))?
            .to_string();
        let id = format!(
            "attachment:{}:{}",
            draft_id.as_str(),
            payload_hash(&path.to_string_lossy())
        );
        let mime = mime_from_filename(&filename).to_string();
        self.db
            .conn()
            .execute(
                "INSERT INTO draft_attachments
                 (id, account_id, draft_id, filename, mime, size_bytes, source_path)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET size_bytes=excluded.size_bytes",
                params![
                    id,
                    account_id.as_str(),
                    draft_id.as_str(),
                    filename,
                    mime,
                    metadata.len() as i64,
                    path.to_string_lossy(),
                ],
            )
            .map_err(sql_err)?;
        Ok(DraftAttachment {
            id,
            account_id: account_id.clone(),
            draft_id: draft_id.clone(),
            filename,
            mime,
            size_bytes: metadata.len(),
            source_path: path.to_path_buf(),
        })
    }

    pub fn list_draft_attachments(
        &self,
        account_id: &AccountId,
        draft_id: &DraftId,
    ) -> Result<Vec<DraftAttachment>, CoreError> {
        self.get_draft(account_id, draft_id)?;
        let mut stmt = self
            .db
            .conn()
            .prepare(
                "SELECT id, account_id, draft_id, filename, mime, size_bytes, source_path
                 FROM draft_attachments WHERE account_id = ?1 AND draft_id = ?2 ORDER BY id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![account_id.as_str(), draft_id.as_str()], |row| {
                Ok(DraftAttachment {
                    id: row.get(0)?,
                    account_id: AccountId(row.get(1)?),
                    draft_id: DraftId(row.get(2)?),
                    filename: row.get(3)?,
                    mime: row.get(4)?,
                    size_bytes: row.get::<_, i64>(5)?.max(0) as u64,
                    source_path: PathBuf::from(row.get::<_, String>(6)?),
                })
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(rows)
    }

    /// Incoming attachment metadata discovered while parsing a cached body
    /// (T-043). This is a Core query: GTK and MCP never inspect SQLite or
    /// MIME files directly. A message outside the requested account is
    /// indistinguishable from a missing one.
    pub fn list_attachments(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
    ) -> Result<Vec<Attachment>, CoreError> {
        self.require_account(account_id.as_str())?;
        let exists: Option<i64> = self
            .db
            .conn()
            .query_row(
                "SELECT 1 FROM messages WHERE id = ?1 AND account_id = ?2",
                params![message_id.as_str(), account_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if exists.is_none() {
            return Err(CoreError::from_code(ErrorCode::MessageNotFound));
        }
        let mut statement = self
            .db
            .conn()
            .prepare(
                "SELECT id, filename, mime, size_bytes, cache_path, content_id,
                        part_path, transfer_encoding
                 FROM attachments
                 WHERE account_id = ?1 AND message_id = ?2
                 ORDER BY id ASC",
            )
            .map_err(sql_err)?;
        let attachments = statement
            .query_map(params![account_id.as_str(), message_id.as_str()], |row| {
                Ok(Attachment {
                    id: AttachmentId(row.get(0)?),
                    account_id: account_id.clone(),
                    message_id: message_id.clone(),
                    filename: row.get(1)?,
                    mime: row.get(2)?,
                    size_bytes: row.get::<_, i64>(3)?.max(0) as u64,
                    cache_path: row.get::<_, Option<String>>(4)?.map(PathBuf::from),
                    content_id: row.get(5)?,
                    part_path: row.get(6)?,
                    transfer_encoding: AttachmentEncoding::parse(&row.get::<_, String>(7)?),
                })
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(attachments)
    }

    /// Looks up one incoming attachment by its local id (T-043). This keeps
    /// the public Core boundary useful for a single MCP/UI action without
    /// making either caller inspect SQLite or MIME data itself.
    pub fn get_attachment(
        &self,
        account_id: &AccountId,
        attachment_id: &AttachmentId,
    ) -> Result<Attachment, CoreError> {
        self.require_account(account_id.as_str())?;
        let attachment = self
            .db
            .conn()
            .query_row(
                "SELECT message_id, filename, mime, size_bytes, cache_path, content_id,
                        part_path, transfer_encoding
                 FROM attachments
                 WHERE id = ?1 AND account_id = ?2",
                params![attachment_id.as_str(), account_id.as_str()],
                |row| {
                    Ok(Attachment {
                        id: attachment_id.clone(),
                        account_id: account_id.clone(),
                        message_id: MessageId(row.get(0)?),
                        filename: row.get(1)?,
                        mime: row.get(2)?,
                        size_bytes: row.get::<_, i64>(3)?.max(0) as u64,
                        cache_path: row.get::<_, Option<String>>(4)?.map(PathBuf::from),
                        content_id: row.get(5)?,
                        part_path: row.get(6)?,
                        transfer_encoding: AttachmentEncoding::parse(&row.get::<_, String>(7)?),
                    })
                },
            )
            .optional()
            .map_err(sql_err)?;
        attachment.ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))
    }

    /// Resolves the local attachment id to the one server folder and IMAP
    /// UID that may fetch it. The background service is the only intended
    /// caller; UI/MCP receive `Attachment` metadata, never this locator.
    pub fn attachment_download_target(
        &self,
        account_id: &AccountId,
        attachment_id: &AttachmentId,
    ) -> Result<AttachmentDownload, CoreError> {
        self.require_account(account_id.as_str())?;
        let row: Option<AttachmentDownloadRow> = self
            .db
            .conn()
            .query_row(
                "SELECT a.message_id, a.filename, a.mime, a.size_bytes,
                        a.cache_path, a.content_id, a.transfer_encoding, a.part_path,
                        m.provider_uid, f.remote_id, a.id, m.account_id
                 FROM attachments a
                 JOIN messages m ON m.id = a.message_id
                 JOIN folders f ON f.id = m.folder_id
                 WHERE a.id = ?1 AND a.account_id = ?2 AND m.account_id = ?2",
                params![attachment_id.as_str(), account_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_err)?;
        let (
            message_id,
            filename,
            mime,
            size_bytes,
            cache_path,
            content_id,
            encoding,
            part_path,
            provider_uid,
            remote_folder,
            id,
            _account,
        ) = row.ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))?;
        let part_path = part_path.filter(|value| !value.is_empty()).ok_or_else(|| {
            CoreError::new(
                ErrorCode::OperationNotSupported,
                "That attachment isn't ready to download yet.",
            )
        })?;
        let transfer_encoding = AttachmentEncoding::parse(&encoding);
        if transfer_encoding == AttachmentEncoding::Unsupported {
            return Err(CoreError::new(
                ErrorCode::OperationNotSupported,
                "That attachment's encoding isn't supported yet.",
            ));
        }
        let remote_folder = remote_folder
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                CoreError::new(
                    ErrorCode::InvalidArgument,
                    "That message's folder hasn't been matched to the server yet.",
                )
            })?;
        let provider_uid = provider_uid
            .and_then(|uid| u32::try_from(uid).ok())
            .ok_or_else(|| {
                CoreError::new(
                    ErrorCode::InvalidArgument,
                    "That message hasn't been synced from the server yet.",
                )
            })?;
        Ok(AttachmentDownload {
            attachment: Attachment {
                id: AttachmentId(id),
                account_id: account_id.clone(),
                message_id: MessageId(message_id),
                filename,
                mime,
                size_bytes: size_bytes.max(0) as u64,
                cache_path: cache_path.map(PathBuf::from),
                content_id,
                part_path: Some(part_path),
                transfer_encoding,
            },
            remote_folder,
            provider_uid,
        })
    }

    /// Commits the relative cache pointer only after service has atomically
    /// finished its streaming write. Absolute/traversing values are refused
    /// so SQLite cannot later escape the configured cache root.
    ///
    /// T-111: this is also where the attachment cache budget is applied, the
    /// same way [`Core::store_body`] applies the body one -- the moment a new
    /// file lands is the only moment the cache can have grown. The size
    /// written to `cache_bytes` is `stat()`ed from the file that was just
    /// accepted rather than taken from `attachments.size_bytes`: the latter
    /// counts the encoded part on the wire, and the file on disk is what the
    /// budget is actually spending. A file that cannot be `stat`ed is
    /// recorded with no size rather than refused -- the pointer is still
    /// true, and the sweep falls back to `size_bytes` for it.
    pub fn mark_attachment_cached(
        &mut self,
        account_id: &AccountId,
        attachment_id: &AttachmentId,
        relative_path: &Path,
        attachments_dir: &Path,
    ) -> Result<(), CoreError> {
        if relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(CoreError::from_code(ErrorCode::InvalidArgument));
        }
        let cached_bytes = std::fs::metadata(attachments_dir.join(relative_path))
            .ok()
            .map(|meta| meta.len() as i64);
        let changed = self
            .db
            .conn()
            .execute(
                "UPDATE attachments SET cache_path = ?1, cache_bytes = ?4 \
                 WHERE id = ?2 AND account_id = ?3",
                params![
                    relative_path.to_string_lossy(),
                    attachment_id.as_str(),
                    account_id.as_str(),
                    cached_bytes
                ],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(CoreError::from_code(ErrorCode::MessageNotFound));
        }
        let limit = self.settings().attachment_cache_limit_bytes;
        self.enforce_attachment_cache_limit_keeping(
            attachments_dir,
            limit,
            Some(attachment_id.as_str()),
        )?;
        Ok(())
    }

    pub fn remove_draft_attachment(
        &self,
        account_id: &AccountId,
        draft_id: &DraftId,
        attachment_id: &str,
    ) -> Result<bool, CoreError> {
        let changed = self
            .db
            .conn()
            .execute(
                "DELETE FROM draft_attachments
                 WHERE account_id = ?1 AND draft_id = ?2 AND id = ?3",
                params![account_id.as_str(), draft_id.as_str(), attachment_id],
            )
            .map_err(sql_err)?;
        Ok(changed != 0)
    }

    /// Freezes a draft into Outbox and queues exactly one SMTP operation.
    /// The queue payload is deliberately empty: the message body stays in
    /// the outbox table and is fetched by the service-side SMTP adapter.
    pub fn queue_draft_send(
        &self,
        account_id: &AccountId,
        draft_id: &DraftId,
    ) -> Result<OperationId, CoreError> {
        self.queue_draft_send_at_revision(account_id, draft_id, None)
    }

    /// Same durable Send door as [`Self::queue_draft_send`], with an optional
    /// revision guard for a user-approved MCP request.  The revision check,
    /// immutable outbox snapshot and queue operation deliberately share one
    /// SQLite transaction: an approval can never freeze a later draft edit.
    pub(crate) fn queue_draft_send_at_revision(
        &self,
        account_id: &AccountId,
        draft_id: &DraftId,
        expected_revision: Option<i64>,
    ) -> Result<OperationId, CoreError> {
        self.require_account(account_id.as_str())?;
        let now = self.now();
        // IMMEDIATE: `queue_draft_send_in` reads the draft and its revision
        // first and only then writes `outbox`. A DEFERRED transaction of that
        // shape fails with SQLITE_BUSY_SNAPSHOT the moment the sync worker
        // (its own `Core::open` on the same file) commits in between -- and
        // that error arrives instantly, ignoring `busy_timeout`, so Send
        // would report "Couldn't save that change." with no retry left.
        let tx = self.db.immediate_transaction_ref().map_err(sql_err)?;
        let operation = queue_draft_send_in(&tx, account_id, draft_id, expected_revision, now)?;
        tx.commit().map_err(sql_err)?;
        Ok(operation)
    }
}

/// Internal half of the send doorway.  MCP authorization invokes this while
/// holding the very same SQLite transaction that checked a confirmation and
/// draft revision, so no later draft can be frozen between those steps.
pub(crate) fn queue_draft_send_in(
    tx: &rusqlite::Transaction<'_>,
    account_id: &AccountId,
    draft_id: &DraftId,
    expected_revision: Option<i64>,
    now: i64,
) -> Result<OperationId, CoreError> {
    let (draft, revision): (Draft, i64) = tx
        .query_row(
            "SELECT id, account_id, thread_id, in_reply_to, from_addr,
                        to_addr, cc, bcc, subject, body, updated_at, remote_uid,
                        sync_revision
                 FROM drafts WHERE account_id = ?1 AND id = ?2",
            params![account_id.as_str(), draft_id.as_str()],
            |row| Ok((map_draft(row)?, row.get(12)?)),
        )
        .optional()
        .map_err(sql_err)?
        .ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))?;
    if expected_revision.is_some_and(|expected| expected != revision) {
        return Err(CoreError::from_code(ErrorCode::PermissionDenied));
    }
    if !crate::recipient_field_is_sendable(&draft.to) {
        return Err(CoreError::new(
            ErrorCode::InvalidArgument,
            if draft.to.trim().is_empty() {
                "Add at least one recipient before sending."
            } else {
                "That doesn’t look like an address."
            },
        ));
    }
    for value in [&draft.cc, &draft.bcc] {
        if !value.trim().is_empty() && !crate::recipient_field_is_sendable(value) {
            return Err(CoreError::new(
                ErrorCode::InvalidArgument,
                "That doesn’t look like an address.",
            ));
        }
    }
    let outbox_id = format!("outbox:{}:{}", draft.id.as_str(), draft.updated_at);
    let reply_headers = draft
            .in_reply_to
            .as_ref()
            .map(|id| {
                tx.query_row(
                    "SELECT message_id_header,
                            trim(COALESCE(references_header, '') || ' ' || COALESCE(message_id_header, ''))
                     FROM messages WHERE account_id = ?1 AND id = ?2",
                    params![account_id.as_str(), id.as_str()],
                    |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(sql_err)
            })
            .transpose()?
            .flatten()
            .unwrap_or((None, String::new()));
    tx.execute(
        "INSERT OR IGNORE INTO outbox
             (id, account_id, draft_id, from_addr, to_addr, cc, bcc,
              subject, body, in_reply_to, references_header, created_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'queued')",
        params![
            outbox_id,
            account_id.as_str(),
            draft.id.as_str(),
            draft.from,
            draft.to,
            draft.cc,
            draft.bcc,
            draft.subject,
            draft.body,
            reply_headers.0,
            if reply_headers.1.is_empty() {
                None
            } else {
                Some(reply_headers.1)
            },
            now,
        ],
    )
    .map_err(sql_err)?;
    tx.execute(
        "INSERT OR IGNORE INTO outbox_attachments
             (outbox_id, filename, mime, size_bytes, source_path)
             SELECT ?1, filename, mime, size_bytes, source_path
             FROM draft_attachments WHERE draft_id = ?2 AND account_id = ?3",
        params![outbox_id, draft.id.as_str(), account_id.as_str()],
    )
    .map_err(sql_err)?;
    let op = enqueue(
        tx,
        account_id.as_str(),
        &outbox_id,
        OpKind::Send,
        "{}",
        None,
        now,
    )?;
    Ok(op)
}

impl Core {
    pub fn load_outbox(
        &self,
        account_id: &AccountId,
        id: &str,
    ) -> Result<OutboxMessage, CoreError> {
        self.require_account(account_id.as_str())?;
        let mut outgoing = self
            .db
            .conn()
            .query_row(
                "SELECT id, account_id, draft_id, from_addr, to_addr, cc, bcc,
                        subject, body, in_reply_to, references_header, status
                 FROM outbox WHERE account_id = ?1 AND id = ?2",
                params![account_id.as_str(), id],
                |row| {
                    Ok(OutboxMessage {
                        id: row.get(0)?,
                        account_id: AccountId(row.get(1)?),
                        draft_id: row.get::<_, Option<String>>(2)?.map(DraftId),
                        from: row.get(3)?,
                        to: row.get(4)?,
                        cc: row.get(5)?,
                        bcc: row.get(6)?,
                        subject: row.get(7)?,
                        body: row.get(8)?,
                        in_reply_to: row.get(9)?,
                        references: row.get(10)?,
                        attachments: Vec::new(),
                        status: row.get(11)?,
                    })
                },
            )
            .optional()
            .map_err(sql_err)?
            .ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))?;
        let mut stmt = self
            .db
            .conn()
            .prepare(
                "SELECT filename, mime, size_bytes, source_path
                 FROM outbox_attachments WHERE outbox_id = ?1 ORDER BY filename, source_path",
            )
            .map_err(sql_err)?;
        outgoing.attachments = stmt
            .query_map(params![id], |row| {
                Ok(OutgoingAttachment {
                    filename: row.get(0)?,
                    mime: row.get(1)?,
                    size_bytes: row.get::<_, i64>(2)?.max(0) as u64,
                    source_path: PathBuf::from(row.get::<_, String>(3)?),
                })
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(outgoing)
    }

    pub fn mark_outbox_sent(&self, account_id: &AccountId, id: &str) -> Result<(), CoreError> {
        let changed = self
            .db
            .conn()
            .execute(
                "UPDATE outbox SET status = 'sent', sent_at = ?3
                 WHERE account_id = ?1 AND id = ?2",
                params![account_id.as_str(), id, self.now()],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            return Err(CoreError::from_code(ErrorCode::MessageNotFound));
        }
        Ok(())
    }

    pub fn mark_outbox_delivered(&self, account_id: &AccountId, id: &str) -> Result<(), CoreError> {
        self.require_account(account_id.as_str())?;
        let changed = self
            .db
            .conn()
            .execute(
                "UPDATE outbox SET status = 'delivered' \
                 WHERE account_id = ?1 AND id = ?2 AND status = 'queued'",
                params![account_id.as_str(), id],
            )
            .map_err(sql_err)?;
        if changed == 0 {
            let status: Option<String> = self
                .db
                .conn()
                .query_row(
                    "SELECT status FROM outbox WHERE account_id=?1 AND id=?2",
                    params![account_id.as_str(), id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_err)?;
            if !matches!(status.as_deref(), Some("delivered" | "sent")) {
                return Err(CoreError::from_code(ErrorCode::MessageNotFound));
            }
        }
        Ok(())
    }

    pub fn sent_remote_folder(&self, account_id: &AccountId) -> Result<Option<String>, CoreError> {
        self.require_account(account_id.as_str())?;
        self.db
            .conn()
            .query_row(
                "SELECT remote_id FROM folders \
                 WHERE account_id=?1 AND kind='sent' AND remote_id IS NOT NULL \
                 ORDER BY id LIMIT 1",
                params![account_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)
    }

    /// The actual server mailbox discovered as `\\Drafts`, if this account
    /// has one. No guessed English name: when this is `None`, T-042 keeps
    /// the local draft and reports the queued operation as unsupported.
    pub fn drafts_remote_folder(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<String>, CoreError> {
        self.require_account(account_id.as_str())?;
        self.db
            .conn()
            .query_row(
                "SELECT remote_id FROM folders \
                 WHERE account_id=?1 AND kind='drafts' \
                   AND remote_id IS NOT NULL AND remote_id <> '' \
                 ORDER BY id LIMIT 1",
                params![account_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)
    }

    /// T-074: sidebar folders for one account, in sidebar order — system
    /// folders (fixed [`FolderKind::SYSTEM_ORDER`]) then custom folders
    /// (alphabetical, for a deterministic sidebar), each with unread/total
    /// counts (D11: one query, not N+1 over threads or folders).
    ///
    /// Snoozed and Starred are *always* overlays over thread flags, never
    /// real rows in `folders` (mirrors [`folder_filter`]'s virtual arms):
    /// on the server these are a flag and a client-local idea, so there is
    /// no mailbox to discover for either one (T-076).
    ///
    /// Archive and Trash used to be the same kind of overlay-only, but
    /// since T-077 a real IMAP `\Archive`/`\Trash` SPECIAL-USE mailbox can
    /// land in `folders` via [`Core::sync_folders`]. Once that row exists
    /// it wins outright and the overlay is suppressed for that kind —
    /// otherwise the sidebar would show two "Archive" rows once a real one
    /// is discovered (T-076). Until then (or for a provider with no
    /// SPECIAL-USE support) the flag overlay is still the fallback, same
    /// as before.
    ///
    /// Inbox/Sent/Drafts/Spam are real rows too; before the sync engine
    /// (T-022 sqq.) creates Sent/Drafts/Spam for an account, they still
    /// show up here (D21 wants a stable sidebar) with a synthesized id and
    /// a zero count, exactly like a folder with no mail yet.
    pub fn list_folders(&self, account_id: &AccountId) -> Result<Vec<FolderSummary>, CoreError> {
        self.require_account(account_id.as_str())?;
        let account = account_id.as_str();
        let conn = self.db.conn();
        let mut stmt = conn.prepare(FOLDER_SUMMARY_SQL).map_err(sql_err)?;
        let rows = stmt
            .query_map(params![account], map_folder_row)
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;

        let mut by_kind: std::collections::HashMap<FolderKind, FolderRow> =
            std::collections::HashMap::new();
        let mut customs: Vec<FolderRow> = Vec::new();
        let mut overlays: Vec<FolderRow> = Vec::new();
        // Real rows first, in their own pass, so a real Archive/Trash
        // folder always wins regardless of the order SQLite happens to
        // emit `UNION ALL` branches in (nothing in the SQL standard or in
        // SQLite's docs promises branch order without an `ORDER BY`, so
        // the suppression rule below must not depend on it).
        for row in rows {
            if row.is_real {
                match FolderKind::parse(&row.kind).unwrap_or(FolderKind::Custom) {
                    FolderKind::Custom => customs.push(row),
                    kind => {
                        by_kind.entry(kind).or_insert(row);
                    }
                }
            } else {
                overlays.push(row);
            }
        }
        // Overlays only fill a kind that no real row already claimed.
        // Starred/Snoozed rows are never claimed by a real row (no
        // provider ever discovers those kinds), so they always land here.
        for row in overlays {
            let kind = FolderKind::parse(&row.kind).unwrap_or(FolderKind::Custom);
            by_kind.entry(kind).or_insert(row);
        }
        customs.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut out = Vec::with_capacity(FolderKind::SYSTEM_ORDER.len() + customs.len());
        for kind in FolderKind::SYSTEM_ORDER {
            out.push(system_summary(kind, by_kind.remove(&kind), account));
        }
        for row in customs {
            out.push(FolderSummary {
                folder: Folder {
                    id: FolderId(row.id),
                    label: row.name,
                    kind: FolderKind::Custom,
                    color: parse_palette_color(row.color.as_deref()),
                    account_id: Some(account_id.clone()),
                    create_failed: row.create_failed,
                },
                unread: nonneg(row.unread),
                total: nonneg(row.total),
            });
        }
        Ok(out)
    }

    /// T-074: local half of creating a folder. The IMAP `CREATE` round trip
    /// is T-025/T-038, not this method — it inserts the row and queues an
    /// [`OpKind::CreateFolder`] operation for that worker to pick up later,
    /// the same way [`Self::dispatch`] queues mail mutations.
    ///
    /// T-084: unlike a thread mutation, a non-retryable `CreateFolder`
    /// failure does **not** delete this row. Two options were on the
    /// table -- (a) delete the local row, mirroring T-081's thread
    /// rollback, or (b) leave the row and make its "not created on the
    /// server" state visible ([`Folder::create_failed`],
    /// `FOLDER_SUMMARY_SQL`). (a) was rejected, not on style but because it
    /// can lose or corrupt data that (b) cannot:
    ///
    /// - `threads.folder_id REFERENCES folders(id)` with no `ON DELETE`
    ///   action, and this database runs `PRAGMA foreign_keys = ON`
    ///   (`feathermail-db`, D13). Nothing stops a user from moving mail
    ///   into a brand new folder before its `CreateFolder` op has even
    ///   been picked up -- `apply_one`'s `Move` arm never checks that the
    ///   target folder is confirmed, only that the row exists, and it
    ///   already does at this point. So the row this method just inserted
    ///   can gain the *only* `threads` rows pointing at it before the
    ///   worker ever hears back from the server. A `DELETE FROM folders`
    ///   on terminal failure would then either hit the FK and error out
    ///   (row stays a ghost anyway, worse: now failing loudly on the wrong
    ///   thing), or -- if this schema ever grew `ON DELETE CASCADE` here --
    ///   silently take the user's mail down with it. (b) never deletes
    ///   anything, so this can't happen either way: the mail just stays
    ///   exactly where the user put it, in a folder now visibly marked as
    ///   not real.
    /// - (b) needs no schema migration and cannot drift from D29/T-076's
    ///   idempotent folder sync: `create_failed` is a read-time projection
    ///   (`remote_id IS NULL` plus a `failed` `operations` row for this
    ///   folder's id), not a stored flag, so a later `sync_folders` walk
    ///   that adopts this same placeholder by name (T-077) clears it for
    ///   free the moment `remote_id` is set -- there is no second place
    ///   that also has to remember to un-mark it.
    /// - What (b) does *not* fix: there is no `delete_folder`/retry command
    ///   yet, and this method's own duplicate-name check means the user
    ///   cannot simply call it again under the same label once the ghost
    ///   exists -- the row and its `failed` operation id
    ///   (`create_folder:{account}:{id}:{hash}`) are permanent until a
    ///   future folder-management command can clear or retry them. That
    ///   gap predates this task (there was no way to remove *any* folder
    ///   before it either) and is left open here.
    pub fn create_folder(
        &mut self,
        account_id: &AccountId,
        name: &str,
    ) -> Result<FolderId, CoreError> {
        self.create_folder_with_color(account_id, name, None)
    }

    /// T-097(11): [`Core::create_folder`] with the dot colour the user
    /// picked. `color` is honoured only when it is one of
    /// [`FOLDER_PALETTE`]'s five values; anything else -- including `None`,
    /// which is every caller that does not offer a picker -- falls back to
    /// the round-robin assignment `create_folder` has always made, so a
    /// caller that does not care keeps getting distinguishable folders.
    pub fn create_folder_with_color(
        &mut self,
        account_id: &AccountId,
        name: &str,
        color: Option<&str>,
    ) -> Result<FolderId, CoreError> {
        self.require_account(account_id.as_str())?;
        let color = parse_palette_color(color);
        let label = name.trim();
        if let Some(err) = crate::folder_label_error(label) {
            return Err(CoreError::new(ErrorCode::InvalidArgument, err.as_str()));
        }
        let slug = folder_slug(label);
        let system_clash = FolderKind::SYSTEM_ORDER
            .iter()
            .any(|kind| kind.as_str() == slug || kind.default_label().eq_ignore_ascii_case(label));
        if system_clash {
            return Err(CoreError::new(
                ErrorCode::InvalidArgument,
                CreateFolderError::SystemName.as_str(),
            ));
        }

        let account = account_id.as_str();
        let full_id = format!("{account}:{slug}");
        let existing: Vec<(String, String, Option<i64>)> = {
            let conn = self.db.conn();
            let mut stmt = conn
                .prepare(
                    "SELECT id, name, deleted_at FROM folders
                     WHERE account_id = ?1 AND kind = 'custom'",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![account], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            rows
        };
        let clashes = |id: &str, name: &str| id == full_id || name.eq_ignore_ascii_case(label);
        // T-060u: a deleted folder's row survives (`messages` and the Undo
        // history reference it by id), so creating that name again must
        // revive the row rather than collide with it. The user asked for a
        // folder with this name; refusing because of an invisible tombstone
        // would be Feather Mail explaining its own bookkeeping to them.
        let tombstone = existing
            .iter()
            .find(|(id, name, deleted_at)| deleted_at.is_some() && clashes(id, name))
            .map(|(id, _, _)| id.clone());
        if let Some(id) = tombstone {
            return self.revive_folder(account_id, &FolderId(id), label, color);
        }
        let duplicate = existing
            .iter()
            .any(|(id, existing_name, _)| clashes(id, existing_name));
        if duplicate {
            return Err(CoreError::new(
                ErrorCode::InvalidArgument,
                CreateFolderError::Duplicate.as_str(),
            ));
        }

        let color = color.unwrap_or(FOLDER_PALETTE[existing.len() % FOLDER_PALETTE.len()]);
        let now = self.now();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        tx.execute(
            "INSERT INTO folders (id, account_id, name, kind, color) VALUES (?1, ?2, ?3, 'custom', ?4)",
            params![full_id, account, label, color],
        )
        .map_err(sql_err)?;
        // `at` is here for the same reason as in `rename_folder`: D29 keys
        // the operation on kind+account+target+payload hash, so recreating a
        // folder that was created and later deleted would otherwise hash to
        // the old acked create and never reach the server.
        let payload = create_folder_payload(label, now);
        enqueue(
            &tx,
            account,
            &full_id,
            OpKind::CreateFolder,
            &payload,
            None,
            now,
        )?;
        tx.commit().map_err(sql_err)?;
        Ok(FolderId(full_id))
    }

    /// T-060u: bring a deleted folder's row back and queue its `CREATE`
    /// again. Called only from [`Core::create_folder`], for the name the
    /// user just asked for.
    ///
    /// `remote_id` is cleared rather than trusted: if the `DELETE` already
    /// acked, the mailbox is gone and the row's old identity is stale; if it
    /// has not, the pending `DELETE` still carries the mailbox name in its
    /// own payload, so it cannot be redirected by this. Either way the next
    /// `CREATE` ack (or `LIST` walk) is what gives the row an identity again.
    fn revive_folder(
        &mut self,
        account_id: &AccountId,
        folder_id: &FolderId,
        label: &str,
        color: Option<&'static str>,
    ) -> Result<FolderId, CoreError> {
        let account = account_id.as_str();
        let now = self.now();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        tx.execute(
            // T-097(11): `COALESCE` so a revive without a picked colour
            // keeps the one the row already had -- the folder comes back
            // looking like itself -- while a picked one is applied.
            "UPDATE folders SET deleted_at = NULL, remote_id = NULL, name = ?1,
                    color = COALESCE(?4, color)
             WHERE account_id = ?2 AND id = ?3",
            params![label, account, folder_id.as_str(), color],
        )
        .map_err(sql_err)?;
        let payload = create_folder_payload(label, now);
        enqueue(
            &tx,
            account,
            folder_id.as_str(),
            OpKind::CreateFolder,
            &payload,
            None,
            now,
        )?;
        tx.commit().map_err(sql_err)?;
        Ok(folder_id.clone())
    }

    /// T-060u: delete one custom folder, locally now and on the server when
    /// the queued `DELETE` lands.
    ///
    /// This is the only folder operation that can destroy mail, so the
    /// contract is deliberately narrow in three ways.
    ///
    /// - **It refuses a folder that still holds mail.** Not because that is
    ///   hard, but because the alternatives are worse: deleting the mail
    ///   with the mailbox makes a tidying gesture irreversible, and quietly
    ///   moving it somewhere is a second bulk operation hidden inside a
    ///   destructive one. The user is told to move the mail first, and both
    ///   halves stay theirs to see.
    /// - **The row is hidden, not removed.** `messages` and the durable Undo
    ///   history reference folders by id, so a `DELETE FROM folders` would
    ///   either fail or take real history with it. `deleted_at` is the only
    ///   truthful local representation.
    /// - **`remote_id` survives until the wire ACK** (see
    ///   `settle_folder_delete`), exactly as in [`Core::rename_folder`]. Until
    ///   then the mailbox still belongs to this row, so a `LIST` walk that
    ///   runs before the `DELETE` cannot resurrect it under a second row --
    ///   and if the `DELETE` fails terminally, the next walk puts the folder
    ///   back with its mail intact.
    ///
    /// Returns whether an IMAP `DELETE` was queued: a folder that never
    /// reached the server has no mailbox to delete, and its pending `CREATE`
    /// is cancelled instead.
    pub fn delete_folder(
        &mut self,
        account_id: &AccountId,
        folder_id: &FolderId,
    ) -> Result<bool, CoreError> {
        self.require_account(account_id.as_str())?;
        let now = self.now();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        let queued = delete_folder_in(&tx, account_id.as_str(), folder_id.as_str(), now)?;
        tx.commit().map_err(sql_err)?;
        Ok(queued)
    }

    /// T-060t: rename one custom folder, locally now and on the server when
    /// the queued `RENAME` lands.
    ///
    /// Only `folders.name` moves optimistically. `remote_id` -- the folder's
    /// *identity*, and what `RemoteLocator::remote_folder` hands to
    /// `SELECT`/`UID MOVE` -- is rewritten solely by the wire ACK, in the
    /// same transaction that marks the operation acked (see
    /// `Core::finish`). That split is the whole design:
    ///
    /// - While the rename is queued, every other operation still resolves
    ///   to the mailbox that actually exists. Rewriting `remote_id` up
    ///   front would point them at a name the server has never heard of.
    /// - If the `RENAME` fails terminally, the local display name is
    ///   briefly wrong and the next `LIST` walk puts it back: identity
    ///   never moved, so `sync_one_folder` finds the same `remote_id` and
    ///   refreshes `name` from the server. A rollback path that had to undo
    ///   an identity change could not promise that.
    ///
    /// The destination path is computed here rather than in the applier
    /// because Core owns both halves of it: the current `remote_id` and the
    /// stored `delimiter`. A nested `Team/Ideas` renamed to "Plans" becomes
    /// `Team/Plans`, not a new top-level `Plans` -- silently promoting a
    /// folder out of its hierarchy is the kind of thing a user discovers
    /// weeks later.
    ///
    /// Returns whether anything was queued: renaming a folder to the name it
    /// already has is a legitimate request with nothing to do, not a failure.
    pub fn rename_folder(
        &mut self,
        account_id: &AccountId,
        folder_id: &FolderId,
        name: &str,
    ) -> Result<bool, CoreError> {
        self.require_account(account_id.as_str())?;
        let account = account_id.as_str();
        let label = name.trim();
        if let Some(err) = crate::folder_label_error(label) {
            return Err(rename_err(match err {
                CreateFolderError::Empty => RenameFolderError::Empty,
                CreateFolderError::InvalidName => RenameFolderError::InvalidName,
                CreateFolderError::SystemName | CreateFolderError::Duplicate => {
                    unreachable!("folder_label_error does not produce those")
                }
            }));
        }
        let slug = folder_slug(label);
        let system_clash = FolderKind::SYSTEM_ORDER
            .iter()
            .any(|kind| kind.as_str() == slug || kind.default_label().eq_ignore_ascii_case(label));
        if system_clash {
            return Err(rename_err(RenameFolderError::SystemName));
        }

        let current: Option<FolderIdentity> = {
            let conn = self.db.conn();
            conn.query_row(
                "SELECT name, kind, remote_id, delimiter, parent_id
                 FROM folders WHERE account_id = ?1 AND id = ?2",
                params![account, folder_id.as_str()],
                |row| {
                    Ok(FolderIdentity {
                        name: row.get(0)?,
                        kind: row.get(1)?,
                        remote_id: row.get(2)?,
                        delimiter: row.get(3)?,
                        parent_id: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(sql_err)?
        };
        let Some(FolderIdentity {
            name: current_name,
            kind,
            remote_id,
            delimiter,
            parent_id,
        }) = current
        else {
            return Err(CoreError::new(
                ErrorCode::InvalidArgument,
                "Folder not found.",
            ));
        };
        if kind != FolderKind::Custom.as_str() {
            return Err(rename_err(RenameFolderError::NotCustom));
        }
        let Some(remote_id) = remote_id.filter(|id| !id.is_empty()) else {
            return Err(rename_err(RenameFolderError::NotOnServer));
        };
        if current_name == label {
            // Already the requested name. Queueing a `RENAME x x` would ask
            // the server to fail on a no-op.
            return Ok(false);
        }

        // Uniqueness is per parent, exactly like the schema's
        // `folders_account_parent_name` index -- two folders called "Ideas"
        // under different parents are two different mailboxes (T-079).
        let duplicate: Option<i64> = {
            let conn = self.db.conn();
            match &parent_id {
                Some(parent) => conn
                    .query_row(
                        "SELECT 1 FROM folders
                         WHERE account_id = ?1 AND parent_id = ?2 AND id <> ?3
                           AND name = ?4 COLLATE NOCASE",
                        params![account, parent, folder_id.as_str(), label],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql_err)?,
                None => conn
                    .query_row(
                        "SELECT 1 FROM folders
                         WHERE account_id = ?1 AND parent_id IS NULL AND id <> ?2
                           AND name = ?3 COLLATE NOCASE",
                        params![account, folder_id.as_str(), label],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql_err)?,
            }
        };
        if duplicate.is_some() {
            return Err(rename_err(RenameFolderError::Duplicate));
        }

        let (parent_remote_id, delimiter) = mailbox_parent(&remote_id, delimiter.as_deref());
        let now = self.now();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        tx.execute(
            "UPDATE folders SET name = ?1 WHERE account_id = ?2 AND id = ?3",
            params![label, account, folder_id.as_str()],
        )
        .map_err(sql_err)?;
        // T-158: the destination goes out as its parts -- the parent path
        // exactly as the server reported it, the delimiter, and the raw
        // label the user typed -- not as one pre-joined string. Joining
        // here meant gluing an *already encoded* prefix to a *not yet
        // encoded* leaf, and the provider then encoded the result as a
        // whole: for a parent «Проекты» the RENAME went out with the
        // prefix escaped a second time, naming a mailbox that does not
        // exist. Only the leaf is ever encoded now, and both sides
        // (`ImapMailProvider`, `queue::settle_folder_rename`) build the
        // path from these fields through `mailbox_remote_id`, so what the
        // server is asked for and what `folders.remote_id` records after
        // the ACK cannot drift apart.
        //
        // `at` is in the payload for one reason: the operation id is
        // kind+account+target+payload hash (D29 dedup), so without it a
        // rename back to a name this folder already had once would hash to
        // the *acked* earlier operation and be silently dropped by
        // `INSERT OR IGNORE`. Two renames within the same second are still
        // the same request and still dedup, which is the case D29 is for.
        let payload = format!(
            r#"{{"from":"{}","parent_remote_id":"{}","delimiter":"{}","label":"{}","at":{}}}"#,
            json_escape(&remote_id),
            json_escape(&parent_remote_id),
            json_escape(&delimiter),
            json_escape(label),
            now
        );
        enqueue(
            &tx,
            account,
            folder_id.as_str(),
            OpKind::RenameFolder,
            &payload,
            None,
            now,
        )?;
        tx.commit().map_err(sql_err)?;
        Ok(true)
    }

    /// T-018: probe IMAP/SMTP, then save the mailbox. Password is not written.
    pub fn add_account(
        &mut self,
        form: &MailboxForm,
        password: &str,
        connector: &impl MailConnector,
    ) -> Result<AccountId, CoreError> {
        if password.is_empty() {
            return Err(CoreError::new(
                ErrorCode::InvalidArgument,
                MailboxFormError::Password.as_str(),
            ));
        }
        self.add_mailbox(form, password, "generic", connector)
    }

    /// T-019: Gmail after OAuth. Access token is probed, not stored in sqlite.
    pub fn add_gmail_account(
        &mut self,
        email: &str,
        access_token: &str,
        connector: &impl MailConnector,
    ) -> Result<AccountId, CoreError> {
        if access_token.is_empty() {
            return Err(CoreError::from_code(ErrorCode::AuthRequired));
        }
        let form = MailboxForm::gmail(email)
            .map_err(|e| CoreError::new(ErrorCode::InvalidArgument, e.as_str()))?;
        self.add_mailbox(&form, access_token, "gmail", connector)
    }

    /// T-165: a Google account whose bearer token comes from the desktop
    /// session's own account manager (GNOME Online Accounts) rather than
    /// from a Feather Mail OAuth client. The mailbox is Gmail's, so the
    /// hosts and the XOAUTH2 probe are `add_gmail_account`'s verbatim --
    /// the *only* difference is the saved `provider` string, which is what
    /// tells `crates/service`'s connector where to get the next token
    /// (`ConnectorKind::Goa`). Nothing Google-issued is stored in sqlite
    /// here either; the token is probed and dropped.
    pub fn add_goa_account(
        &mut self,
        email: &str,
        access_token: &str,
        connector: &impl MailConnector,
    ) -> Result<AccountId, CoreError> {
        if access_token.is_empty() {
            return Err(CoreError::from_code(ErrorCode::AuthRequired));
        }
        let form = MailboxForm::gmail(email)
            .map_err(|e| CoreError::new(ErrorCode::InvalidArgument, e.as_str()))?;
        self.add_mailbox(&form, access_token, "goa", connector)
    }

    /// T-020: Microsoft after OAuth. Access token is probed, not stored in sqlite.
    pub fn add_microsoft_account(
        &mut self,
        email: &str,
        access_token: &str,
        connector: &impl MailConnector,
    ) -> Result<AccountId, CoreError> {
        if access_token.is_empty() {
            return Err(CoreError::from_code(ErrorCode::AuthRequired));
        }
        let form = MailboxForm::microsoft(email)
            .map_err(|e| CoreError::new(ErrorCode::InvalidArgument, e.as_str()))?;
        self.add_mailbox(&form, access_token, "microsoft", connector)
    }

    fn add_mailbox(
        &mut self,
        form: &MailboxForm,
        secret: &str,
        provider: &str,
        connector: &impl MailConnector,
    ) -> Result<AccountId, CoreError> {
        let email = form.email.trim();
        let taken: Vec<String> = {
            let conn = self.db.conn();
            let mut stmt = conn
                .prepare("SELECT id, email FROM accounts")
                .map_err(sql_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(sql_err)?;
            let mut ids = Vec::new();
            for row in rows {
                let (id, existing) = row.map_err(sql_err)?;
                if existing.eq_ignore_ascii_case(email) {
                    return Err(CoreError::new(
                        ErrorCode::InvalidArgument,
                        AddAccountError::Duplicate.as_str(),
                    ));
                }
                ids.push(id);
            }
            ids
        };
        let account_id = unique_account_id(email, taken.iter().map(String::as_str));
        let _probe = connector.probe(form, secret)?;
        let now = self.now();
        let name = form.display_name();
        let folder_id = format!("{}:inbox", account_id.as_str());
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        tx.execute(
            "INSERT INTO accounts (
                id, name, email, provider,
                imap_host, imap_port, smtp_host, smtp_port,
                imap_security, smtp_security, username,
                status, download_policy, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'synced', 'recent', ?12, ?12)",
            params![
                account_id.as_str(),
                name,
                email,
                provider,
                form.imap_host,
                form.imap_port as i64,
                form.smtp_host,
                form.smtp_port as i64,
                form.imap_security.as_str(),
                form.smtp_security.as_str(),
                email,
                now,
            ],
        )
        .map_err(sql_err)?;
        tx.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES (?1, ?2, 'Inbox', 'inbox')",
            params![folder_id, account_id.as_str()],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(account_id)
    }

    /// T-021: delete an account and everything scoped to it — folders,
    /// threads, messages, attachments, drafts, snoozes, sync state, the
    /// `operations` queue, and mcp audit rows. One SQLite transaction,
    /// driven off [`feathermail_db::Database::tables_with_account_id`]
    /// rather than a hand-maintained list, so a future table with an
    /// `account_id` column is swept up too instead of becoming an orphan.
    /// An unknown `account_id` is `ErrorCode::AccountNotFound`.
    ///
    /// `messages_fts` has no `account_id` column (it is an fts5 virtual
    /// table indexed by `message_id`, i.e. `messages.id`), so it is not in
    /// `tables_with_account_id`'s sweep and needs its own statement here,
    /// run *before* `messages` rows are deleted (the join target has to
    /// still exist). `feathermail_db`'s
    /// `every_table_is_accounted_for_by_remove_account_or_an_explicit_reason`
    /// test is the guard that a future FTS/cache table can't repeat this
    /// silently: it fails until a new table is deliberately classified.
    ///
    /// Keyring cleanup runs *after* the local transaction commits and is
    /// best-effort: a locked or unreachable keyring must not trap local
    /// data behind it (D14 already refuses to *add* an account without a
    /// keyring; once added, a keyring that later goes away must not stop
    /// removal). The failure — never the secret — comes back in
    /// [`RemoveAccountReport::keyring_error`]; local removal has already
    /// succeeded by the time the caller sees it.
    ///
    /// T-024: cached message bodies for the removed account live on disk
    /// under `crate::body::default_bodies_dir()` (a sibling of `mail.db`,
    /// *not* `~/.cache/feathermail/` — an earlier version of this comment
    /// assumed a cache location nothing ever actually wrote to; the real
    /// layout was decided in `crate::body`, see its module doc). Their
    /// relative paths are read out of `messages.body_path` before the
    /// per-table sweep below deletes those rows, and the files are removed
    /// (best-effort — a file already missing, e.g. through the self-heal
    /// path in `Core::lookup_body`, is not an error) only *after* the
    /// transaction commits, so a crash mid-cleanup never leaves the SQLite
    /// side inconsistent with a partially-swept cache. Attachment cache
    /// files are unaffected: `crates/attachments` writes nothing yet (still
    /// true as of this task), so there is nothing there to purge.
    pub fn remove_account(
        &mut self,
        account_id: &AccountId,
        secrets: &impl SecretStore,
    ) -> Result<RemoveAccountReport, CoreError> {
        self.remove_account_in(account_id, secrets, &crate::body::default_bodies_dir())
    }

    /// Same as [`Core::remove_account`], but takes the body-cache directory
    /// explicitly instead of assuming [`crate::body::default_bodies_dir`].
    /// [`Core::remove_account`] is kept as a two-argument method (rather
    /// than changing its signature to take a path) because `crates/app`
    /// already calls it and is off limits for this task; this is the real
    /// entry point tests reach for so they can point at a tempdir instead
    /// of a real home directory.
    pub fn remove_account_in(
        &mut self,
        account_id: &AccountId,
        secrets: &impl SecretStore,
        bodies_dir: &Path,
    ) -> Result<RemoveAccountReport, CoreError> {
        self.require_account(account_id.as_str())?;
        let id = account_id.as_str();
        let tables = self.db.tables_with_account_id().map_err(db_err)?;
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        // Deletes below run table-by-table in whatever order sqlite_master
        // happens to return, which does not match the schema's foreign-key
        // dependency order (e.g. `folders` before the `threads`/`messages`
        // rows still pointing at it). Deferring FK checks to COMMIT is safe
        // here because every statement filters by the *same* account_id, so
        // the row set removed from each table never leaves a reference into
        // another table dangling for this account by the time we commit.
        tx.execute_batch("PRAGMA defer_foreign_keys = ON")
            .map_err(sql_err)?;
        // Must run before the `messages` delete below: the subquery needs
        // that account's message ids to resolve their mapped FTS rowids.
        // `message_id` is UNINDEXED inside FTS5, so targeting the ordinary
        // map first keeps account removal proportional to its own rows.
        tx.execute(
            "DELETE FROM messages_fts WHERE rowid IN \
             (SELECT fts_rowid FROM fts_message_rows WHERE message_id IN \
              (SELECT id FROM messages WHERE account_id = ?1))",
            params![id],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM operation_move_history
             WHERE operation_id IN (SELECT id FROM operations WHERE account_id = ?1)",
            params![id],
        )
        .map_err(sql_err)?;
        // Read the cached-body relative paths out before the sweep below
        // deletes the rows that point at them (the `messages` DELETE is
        // one of the `tables` this loop runs).
        let body_paths: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT body_path FROM messages \
                     WHERE account_id = ?1 AND body_path IS NOT NULL",
                )
                .map_err(sql_err)?;
            let out = stmt
                .query_map(params![id], |row| row.get(0))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<String>>>()
                .map_err(sql_err)?;
            out
        };
        for table in &tables {
            tx.execute(
                &format!("DELETE FROM {table} WHERE account_id = ?1"),
                params![id],
            )
            .map_err(sql_err)?;
        }
        tx.execute("DELETE FROM accounts WHERE id = ?1", params![id])
            .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;

        // Best-effort, and deliberately *after* the commit above: an
        // account with zero cached bodies (`body_paths` empty) must not
        // fail here, and neither should a body file that is already gone
        // for any reason.
        for rel in &body_paths {
            let _ = std::fs::remove_file(bodies_dir.join(rel));
        }

        let keyring_error = secrets.delete_account(id).err().map(|e| e.to_string());
        Ok(RemoveAccountReport { keyring_error })
    }

    /// T-021 (§24 Accounts): edit an existing account's display name and,
    /// for `provider = "generic"`, its IMAP/SMTP host/port/security. The
    /// email address is the account's identifier and has no field on
    /// [`AccountEdit`] — it cannot be changed here.
    ///
    /// A display-name-only edit is written directly, no network round trip.
    /// Any edit that touches the connection (host/port/security or a new
    /// password — see [`AccountEdit::touches_connection`]) is probed with
    /// `connector` first, exactly like [`Self::add_account`]; on failure
    /// nothing in the row changes. OAuth accounts (`gmail`/`microsoft`)
    /// reject a connection-touching edit with `ErrorCode::InvalidArgument`
    /// — those hosts are provider-managed, not user text fields.
    pub fn update_account(
        &mut self,
        account_id: &AccountId,
        edit: &AccountEdit,
        connector: &impl MailConnector,
    ) -> Result<(), CoreError> {
        self.require_account(account_id.as_str())?;
        let id = account_id.as_str();
        let current = self.load_account_row(id)?;

        let server_edit = edit.touches_connection();
        if server_edit && current.provider != "generic" {
            return Err(CoreError::new(
                ErrorCode::InvalidArgument,
                "Google and Microsoft accounts manage their server settings automatically.",
            ));
        }

        let name = match &edit.display_name {
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    return Err(CoreError::new(
                        ErrorCode::InvalidArgument,
                        "Enter a display name.",
                    ));
                }
                Some(trimmed.to_string())
            }
            None => None,
        };

        if !server_edit {
            if let Some(name) = name {
                let now = self.now();
                self.db
                    .conn()
                    .execute(
                        "UPDATE accounts SET name = ?1, updated_at = ?2 WHERE id = ?3",
                        params![name, now, id],
                    )
                    .map_err(sql_err)?;
            }
            return Ok(());
        }

        let secret = edit
            .new_password
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CoreError::new(
                    ErrorCode::InvalidArgument,
                    MailboxFormError::Password.as_str(),
                )
            })?;

        let imap_host = match &edit.imap_host {
            Some(h) => normalize_host(h).ok_or_else(|| {
                CoreError::new(
                    ErrorCode::InvalidArgument,
                    MailboxFormError::ImapHost.as_str(),
                )
            })?,
            None => current.imap_host.clone(),
        };
        let smtp_host = match &edit.smtp_host {
            Some(h) => normalize_host(h).ok_or_else(|| {
                CoreError::new(
                    ErrorCode::InvalidArgument,
                    MailboxFormError::SmtpHost.as_str(),
                )
            })?,
            None => current.smtp_host.clone(),
        };
        let imap_port = match edit.imap_port {
            Some(0) => {
                return Err(CoreError::new(
                    ErrorCode::InvalidArgument,
                    MailboxFormError::ImapPort.as_str(),
                ))
            }
            Some(p) => p,
            None => current.imap_port,
        };
        let smtp_port = match edit.smtp_port {
            Some(0) => {
                return Err(CoreError::new(
                    ErrorCode::InvalidArgument,
                    MailboxFormError::SmtpPort.as_str(),
                ))
            }
            Some(p) => p,
            None => current.smtp_port,
        };
        let imap_security = edit.imap_security.unwrap_or(current.imap_security);
        let smtp_security = edit.smtp_security.unwrap_or(current.smtp_security);

        let form = MailboxForm {
            email: current.email.clone(),
            imap_host,
            imap_port,
            imap_security,
            smtp_host,
            smtp_port,
            smtp_security,
        };
        let _probe = connector.probe(&form, secret)?;

        let now = self.now();
        let display_name = name.unwrap_or_else(|| current.name.clone());
        self.db
            .conn()
            .execute(
                "UPDATE accounts SET name = ?1, imap_host = ?2, imap_port = ?3, smtp_host = ?4, \
                 smtp_port = ?5, imap_security = ?6, smtp_security = ?7, updated_at = ?8 \
                 WHERE id = ?9",
                params![
                    display_name,
                    form.imap_host,
                    form.imap_port as i64,
                    form.smtp_host,
                    form.smtp_port as i64,
                    form.imap_security.as_str(),
                    form.smtp_security.as_str(),
                    now,
                    id,
                ],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    fn load_account_row(&self, id: &str) -> Result<AccountRow, CoreError> {
        self.db
            .conn()
            .query_row(
                "SELECT provider, name, email, imap_host, imap_port, smtp_host, smtp_port,
                        imap_security, smtp_security
                 FROM accounts WHERE id = ?1",
                params![id],
                |row| {
                    Ok(AccountRow {
                        provider: row.get(0)?,
                        name: row.get(1)?,
                        email: row.get(2)?,
                        imap_host: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        imap_port: row.get::<_, Option<i64>>(4)?.unwrap_or(0) as u16,
                        smtp_host: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                        smtp_port: row.get::<_, Option<i64>>(6)?.unwrap_or(0) as u16,
                        imap_security: row
                            .get::<_, Option<String>>(7)?
                            .and_then(|s| MailSecurity::parse(&s))
                            .unwrap_or(MailSecurity::StartTls),
                        smtp_security: row
                            .get::<_, Option<String>>(8)?
                            .and_then(|s| MailSecurity::parse(&s))
                            .unwrap_or(MailSecurity::StartTls),
                    })
                },
            )
            .optional()
            .map_err(sql_err)?
            .ok_or_else(|| CoreError::from_code(ErrorCode::AccountNotFound))
    }

    /// T-060s: record "sync this account now" so a process that holds no
    /// `SyncHandle` can still ask for one.
    ///
    /// The stdio MCP server is a separate process (see
    /// `crates/mcp/src/main.rs`); the sync worker's channel lives in the
    /// GTK shell. SQLite is the only thing both of them touch, so the
    /// request is durable state, not a signal: if no shell is running the
    /// row simply waits, and the next shell to poll claims it. Repeated
    /// calls collapse onto one row on purpose -- two agents asking a
    /// second apart want one sync.
    ///
    /// An unknown account is `ErrorCode::AccountNotFound`, so a caller
    /// cannot use this to learn whether an id exists behind its allowlist.
    pub fn request_account_sync(&mut self, account_id: &AccountId) -> Result<(), CoreError> {
        self.require_account(account_id.as_str())?;
        let now = self.now();
        self.db
            .conn()
            .execute(
                "INSERT INTO sync_requests (account_id, requested_at) VALUES (?1, ?2)
                 ON CONFLICT(account_id) DO UPDATE SET requested_at = excluded.requested_at",
                params![account_id.as_str(), now],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Claims up to `limit` pending sync requests, deleting them in the
    /// same transaction that reads them.
    ///
    /// Claim-and-delete, not read-then-delete: the shell polls this twice a
    /// second, and a request that survived its own claim would wake the
    /// worker again on every tick. Deleting means a request is honoured
    /// exactly once -- and losing one to a crash between the delete and the
    /// wake costs an agent one `sync_account` call, while leaking one would
    /// cost the user a permanent IMAP-polling loop.
    pub fn take_sync_requests(&mut self, limit: usize) -> Result<Vec<AccountId>, CoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        let claimed: Vec<String> = {
            let mut stmt = tx
                .prepare("SELECT account_id FROM sync_requests ORDER BY requested_at, account_id LIMIT ?1")
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![limit as i64], |row| row.get::<_, String>(0))
                .map_err(sql_err)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql_err)?
        };
        for id in &claimed {
            tx.execute(
                "DELETE FROM sync_requests WHERE account_id = ?1",
                params![id],
            )
            .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;
        Ok(claimed.into_iter().map(AccountId).collect())
    }

    pub(crate) fn require_account(&self, id: &str) -> Result<(), CoreError> {
        let n: i64 = self
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM accounts WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        if n == 0 {
            Err(CoreError::from_code(ErrorCode::AccountNotFound))
        } else {
            Ok(())
        }
    }

    pub(crate) fn now(&self) -> i64 {
        self.now.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        })
    }

    pub(crate) fn emit(&mut self, event: MailEvent) {
        self.listeners.retain(|tx| tx.send(event.clone()).is_ok());
    }
}

/// The next free `draft:{account}:{n}` number for this account.
///
/// Only ids whose tail is *entirely* digits, and short enough that the CAST
/// cannot saturate, are counted. Both guards matter: `drafts.id` is not
/// Core's alone -- MCP passes `draft_id` through unvalidated and writes ids
/// like `draft:{acc}:send-email:{digest}` itself -- so one planted row such
/// as `draft:{acc}:99999999999999999999` used to saturate the CAST at
/// `i64::MAX`, make `MAX(...) + 1` come back as a REAL, and fail the whole
/// query. The `+ 1` moved out of SQL for the same reason: overflow is an
/// error here, never a silent change of type.
fn next_draft_sequence(conn: &rusqlite::Connection, account: &str) -> rusqlite::Result<i64> {
    let max: i64 = conn.query_row(
        "SELECT COALESCE(MAX(CAST(substr(id, length(?1) + 8) AS INTEGER)), 0)
         FROM drafts
         WHERE account_id = ?1
           AND id LIKE 'draft:' || ?1 || ':%'
           AND substr(id, length(?1) + 8) GLOB '[0-9]*'
           AND NOT substr(id, length(?1) + 8) GLOB '*[^0-9]*'
           AND length(id) - length(?1) - 7 <= 18",
        params![account],
        |row| row.get(0),
    )?;
    max.checked_add(1)
        .ok_or(rusqlite::Error::IntegralValueOutOfRange(0, max))
}

fn mime_from_filename(filename: &str) -> &'static str {
    match Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "txt" | "md" => "text/plain",
        "csv" => "text/csv",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

fn push_address_suggestion(
    output: &mut Vec<Address>,
    seen: &mut std::collections::HashSet<String>,
    needle: &str,
    name: &str,
    email: &str,
    limit: usize,
) {
    let email = email.trim();
    if output.len() >= limit || !email.contains('@') {
        return;
    }
    let key = email.to_ascii_lowercase();
    if (!key.contains(needle) && !name.to_ascii_lowercase().contains(needle)) || !seen.insert(key) {
        return;
    }
    output.push(Address {
        name: name.trim().to_string(),
        email: email.to_string(),
    });
}

/// T-101: the Subject a Reply/Reply all/Forward draft opens with.
///
/// Decodes RFC 2047 first. `messages.subject` is *supposed* to hold display
/// text -- `CoreSyncStore::upsert_one` decodes every header exactly once on
/// the way in -- but the list and the reading pane decode again when they
/// paint (`display_subject`), so a row that predates that sync path, or that
/// arrived by any other door, still looks right on screen while holding
/// `=?UTF-8?B?...?=`. Nothing decoded on the way *out*: the owner replied to
/// a Cyrillic message and the compose window's Subject field was "a set of
/// incomprehensible signs". Decoding here is idempotent -- text with no
/// encoded word in it comes back unchanged -- and it is also what the `Re:`
/// test below must see, since an encoded subject that already says "Re:"
/// hides it inside base64.
fn response_subject(prefix: &str, subject: &str) -> String {
    let subject = decode_encoded_words(subject);
    let subject = subject.as_str();
    if subject
        .trim_start()
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
    {
        subject.to_string()
    } else if subject.trim().is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix} {subject}")
    }
}

/// Keeps original display strings when possible, while comparing only the
/// address portion to avoid placing the current account, the primary Reply
/// recipient, or duplicate recipients in Reply All's Cc field.
fn reply_all_cc(
    original_to: &str,
    original_cc: &str,
    account_email: &str,
    sender_email: &str,
) -> String {
    let excluded = [account_email, sender_email]
        .into_iter()
        .map(normalized_recipient)
        .collect::<std::collections::HashSet<_>>();
    let mut seen = std::collections::HashSet::new();
    [original_to, original_cc]
        .into_iter()
        .flat_map(|raw| raw.split([',', ';']))
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .filter(|raw| {
            let normalized = normalized_recipient(raw);
            !normalized.is_empty() && !excluded.contains(&normalized) && seen.insert(normalized)
        })
        .map(str::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn normalized_recipient(raw: &str) -> String {
    let trimmed = raw.trim();
    let address = trimmed
        .rsplit_once('<')
        .map(|(_, tail)| tail.trim_end_matches('>').trim())
        .unwrap_or_else(|| trimmed.split_whitespace().last().unwrap_or_default());
    address.trim_matches(['<', '>', '"']).to_ascii_lowercase()
}

fn map_draft(row: &rusqlite::Row<'_>) -> rusqlite::Result<Draft> {
    Ok(Draft {
        id: DraftId(row.get(0)?),
        account_id: AccountId(row.get(1)?),
        thread_id: row.get::<_, Option<String>>(2)?.map(ThreadId),
        in_reply_to: row.get::<_, Option<String>>(3)?.map(MessageId),
        from: row.get(4)?,
        to: row.get(5)?,
        cc: row.get(6)?,
        bcc: row.get(7)?,
        subject: row.get(8)?,
        body: row.get(9)?,
        updated_at: row.get(10)?,
        remote_uid: row
            .get::<_, Option<i64>>(11)?
            .and_then(|uid| u32::try_from(uid).ok()),
    })
}

fn map_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    let status: String = row.get(3)?;
    Ok(Account {
        id: AccountId(row.get(0)?),
        name: row.get(1)?,
        email: row.get(2)?,
        // Unrecognized status text reads back as Offline, not the enum's
        // own default (Synced) — a garbled column should never claim to be
        // caught up (T-074).
        status: AccountStatus::parse(&status).unwrap_or(AccountStatus::Offline),
    })
}

/// D21 palette for custom folder dots. Deliberately duplicated from
/// `fake.rs`'s private constant of the same values rather than shared,
/// since `Folder::color` is `Option<&'static str>` and `fake.rs` is off
/// limits to edit for this task.
///
/// T-097(11): public now, because the New-folder popover offers these five
/// swatches. The picker must show exactly the colours Core will accept --
/// a sixth swatch in the GTK layer would be a colour Core silently drops.
pub const FOLDER_PALETTE: [&str; 5] = ["#47CC50", "#9451F4", "#FB954A", "#4181F3", "#2DD2E0"];

/// One row out of [`FOLDER_SUMMARY_SQL`]: either a real `folders` row
/// (`is_real`) or one of the four synthesized virtual-folder aggregates.
struct FolderRow {
    id: String,
    name: String,
    kind: String,
    color: Option<String>,
    unread: i64,
    total: i64,
    is_real: bool,
    create_failed: bool,
}

fn map_folder_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FolderRow> {
    Ok(FolderRow {
        id: row.get(0)?,
        name: row.get(1)?,
        kind: row.get(2)?,
        color: row.get(3)?,
        unread: row.get(4)?,
        total: row.get(5)?,
        is_real: row.get(6)?,
        create_failed: row.get(7)?,
    })
}

/// D11: every system-folder count and every real folder row for the
/// account, in one round trip. The four `UNION ALL` branches mirror
/// [`folder_filter`]'s virtual arms exactly (archive/trash/snoozed/starred
/// are thread-flag overlays), so a count here can never drift from what
/// `list_threads` would actually return for that folder. The trailing
/// `1`/`0` literal (`is_real`) tags which half of the union a row came
/// from — since T-077, `archive`/`trash` can also be real `folders` rows
/// (a discovered `\Archive`/`\Trash` SPECIAL-USE mailbox), so `kind` alone
/// no longer tells real and overlay apart the way the old comment assumed.
///
/// The final column is [`Folder::create_failed`] (T-084): for a real row,
/// true exactly when nothing backs it on the server yet
/// (`remote_id IS NULL`) *and* its own `OpKind::CreateFolder` operation is
/// sitting in `operations` as `failed` -- the queue only reaches that
/// state through a non-retryable [`crate::provider::ApplyError`]
/// (`Core::queue::apply_claimed`), never a transient one, so this is a
/// terminal signal, not "still trying." The `remote_id IS NULL` half
/// matters on its own: if a later `sync_folders` walk adopts this same
/// placeholder row by name (T-077's placeholder-adoption case), the row
/// stops being a ghost even though the *original* operation row is still
/// sitting there `failed` -- nothing ever goes back and touches that op,
/// so `remote_id` is what has to gate the flag, not "operation exists."
/// Virtual overlay branches hardcode `0`: none of them is a `folders` row,
/// so no `CreateFolder` operation could ever target one.
const FOLDER_SUMMARY_SQL: &str = "\
WITH counts AS (
    SELECT folder_id,
           SUM(CASE WHEN unread = 1 THEN 1 ELSE 0 END) AS unread,
           COUNT(*) AS total
    FROM threads
    WHERE account_id = ?1 AND archived = 0 AND deleted = 0 AND snooze_until IS NULL
    GROUP BY folder_id
)
SELECT f.id, f.name, f.kind, f.color,
       COALESCE(c.unread, 0), COALESCE(c.total, 0), 1,
       CASE WHEN f.remote_id IS NULL AND EXISTS (
           SELECT 1 FROM operations o
           WHERE o.account_id = f.account_id
             AND o.target_id = f.id
             AND o.op = 'create_folder'
             AND o.status = 'failed'
       ) THEN 1 ELSE 0 END
FROM folders f
LEFT JOIN counts c ON c.folder_id = f.id
WHERE f.account_id = ?1 AND f.deleted_at IS NULL

UNION ALL
SELECT 'starred', 'Starred', 'starred', NULL,
       COALESCE(SUM(CASE WHEN unread = 1 THEN 1 ELSE 0 END), 0), COUNT(*), 0, 0
FROM threads
WHERE account_id = ?1 AND archived = 0 AND deleted = 0 AND snooze_until IS NULL AND starred = 1

UNION ALL
SELECT 'snoozed', 'Snoozed', 'snoozed', NULL,
       COALESCE(SUM(CASE WHEN unread = 1 THEN 1 ELSE 0 END), 0), COUNT(*), 0, 0
FROM threads
WHERE account_id = ?1 AND archived = 0 AND deleted = 0 AND snooze_until IS NOT NULL

UNION ALL
SELECT 'archive', 'Archive', 'archive', NULL,
       COALESCE(SUM(CASE WHEN unread = 1 THEN 1 ELSE 0 END), 0), COUNT(*), 0, 0
FROM threads
WHERE account_id = ?1 AND archived = 1 AND deleted = 0

UNION ALL
SELECT 'trash', 'Trash', 'trash', NULL,
       COALESCE(SUM(CASE WHEN unread = 1 THEN 1 ELSE 0 END), 0), COUNT(*), 0, 0
FROM threads
WHERE account_id = ?1 AND deleted = 1
";

fn nonneg(n: i64) -> u32 {
    n.max(0) as u32
}

fn parse_palette_color(raw: Option<&str>) -> Option<&'static str> {
    let raw = raw?;
    FOLDER_PALETTE.iter().copied().find(|c| *c == raw)
}

/// System-folder [`FolderSummary`] for one [`FolderKind`]: the real row if
/// `list_folders` found one (e.g. Inbox, or Sent/Drafts/Spam once T-022+
/// has synced them), otherwise a zero-count placeholder so the sidebar
/// order never changes shape as sync fills in.
fn system_summary(kind: FolderKind, row: Option<FolderRow>, account: &str) -> FolderSummary {
    match row {
        Some(row) => FolderSummary {
            folder: Folder {
                id: FolderId(row.id),
                label: row.name,
                kind,
                color: None,
                account_id: None,
                create_failed: row.create_failed,
            },
            unread: nonneg(row.unread),
            total: nonneg(row.total),
        },
        None => FolderSummary {
            folder: Folder {
                id: FolderId(format!("{account}:{}", kind.as_str())),
                label: kind.default_label().to_string(),
                kind,
                color: None,
                account_id: None,
                create_failed: false,
            },
            unread: 0,
            total: 0,
        },
    }
}

/// Same normalization `fake.rs`'s private `slug` helper uses for custom
/// folder ids, duplicated here for the same off-limits-to-edit reason as
/// [`FOLDER_PALETTE`].
fn folder_slug(label: &str) -> String {
    label
        .trim()
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

/// The `folders` row as [`Core::rename_folder`] needs it: the display name it
/// is about to change, plus everything that decides whether it may and what
/// the destination mailbox path is.
struct FolderIdentity {
    name: String,
    kind: String,
    remote_id: Option<String>,
    delimiter: Option<String>,
    parent_id: Option<String>,
}

/// One `create_folder` operation payload. See [`Core::create_folder`] for why
/// `at` is part of it.
/// The body of [`Core::delete_folder`], written against a caller-owned
/// transaction so the MCP door (`Core::queue_mcp_delete_folder`) can join
/// the deletion to the same transaction that consumes its one-shot GTK
/// approval. Splitting them would mean a window where an approval is spent
/// and the folder is not deleted, or the reverse.
///
/// The caller has already checked the account exists; everything else --
/// including the emptiness rule, which is the whole point -- is decided
/// here, inside the transaction, so nothing can change between the check
/// and the write.
pub(crate) fn delete_folder_in(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    folder_id: &str,
    now: i64,
) -> Result<bool, CoreError> {
    let current: Option<(String, Option<String>, Option<i64>)> = tx
        .query_row(
            "SELECT kind, remote_id, deleted_at FROM folders
             WHERE account_id = ?1 AND id = ?2",
            params![account, folder_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((kind, remote_id, deleted_at)) = current else {
        return Err(CoreError::new(
            ErrorCode::InvalidArgument,
            "Folder not found.",
        ));
    };
    if kind != FolderKind::Custom.as_str() {
        return Err(delete_err(DeleteFolderError::NotCustom));
    }
    if deleted_at.is_some() {
        // Already deleted. Saying so plainly beats queueing a second
        // `DELETE` for a mailbox this row may no longer even own.
        return Ok(false);
    }
    // Every thread counts, including archived, trashed and snoozed ones:
    // "empty" has to mean the mailbox is empty, not that its unread list
    // looks empty in the sidebar.
    let mail: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM threads WHERE account_id = ?1 AND folder_id = ?2",
            params![account, folder_id],
            |row| row.get(0),
        )
        .map_err(sql_err)?;
    if mail > 0 {
        return Err(delete_err(DeleteFolderError::NotEmpty));
    }

    tx.execute(
        "UPDATE folders SET deleted_at = ?1 WHERE account_id = ?2 AND id = ?3",
        params![now, account, folder_id],
    )
    .map_err(sql_err)?;
    let Some(mailbox) = remote_id.filter(|id| !id.is_empty()) else {
        // Never reached the server. There is no mailbox to delete, and
        // the queued `CREATE` for it must not run afterwards and put one
        // there -- so it is cancelled in the same transaction.
        tx.execute(
            "UPDATE operations SET status = 'cancelled', next_attempt_at = NULL
             WHERE account_id = ?1 AND target_id = ?2 AND op = 'create_folder'
               AND status IN ('local', 'pending', 'failed')",
            params![account, folder_id],
        )
        .map_err(sql_err)?;
        return Ok(false);
    };
    // The mailbox name is frozen into the payload: a server-side rename
    // between now and the ACK must not redirect this `DELETE` at
    // whatever mailbox the row points at by then. `at` keeps D29 dedup
    // from swallowing a delete of a name that was deleted once before.
    let payload = format!(r#"{{"mailbox":"{}","at":{}}}"#, json_escape(&mailbox), now);
    enqueue(
        tx,
        account,
        folder_id,
        OpKind::DeleteFolder,
        &payload,
        None,
        now,
    )?;
    Ok(true)
}

fn create_folder_payload(label: &str, now: i64) -> String {
    format!(r#"{{"name":"{}","at":{}}}"#, json_escape(label), now)
}

fn delete_err(err: DeleteFolderError) -> CoreError {
    CoreError::new(ErrorCode::InvalidArgument, err.as_str())
}

fn rename_err(err: RenameFolderError) -> CoreError {
    CoreError::new(ErrorCode::InvalidArgument, err.as_str())
}

/// Split a mailbox path into the hierarchy prefix its leaf hangs off and
/// the delimiter that separates the two.
///
/// `delimiter` is the server's own separator as stored in
/// `folders.delimiter`; with none reported, or none actually present in
/// this path, the folder is at the top of the namespace and both halves
/// are empty -- which is why the delimiter is returned rather than assumed
/// by the caller: `"/Ideas"` under `/` has an *empty* prefix and still
/// needs its leading slash, and `"Ideas"` under `/` has no prefix at all
/// and must not gain one. A multi-character `delimiter` value cannot come
/// from IMAP (`LIST` reports a single character) but is handled by
/// `rsplit_once` anyway rather than indexed into.
pub(crate) fn mailbox_parent(remote_id: &str, delimiter: Option<&str>) -> (String, String) {
    match delimiter
        .filter(|d| !d.is_empty())
        .and_then(|d| remote_id.rsplit_once(d).map(|(prefix, _leaf)| (prefix, d)))
    {
        Some((prefix, delim)) => (prefix.to_string(), delim.to_string()),
        None => (String::new(), String::new()),
    }
}

/// T-158: the mailbox path a folder has once its leaf is `label`, ready to
/// go on the wire.
///
/// Only the leaf is encoded to modified UTF-7. `parent_remote_id` is the
/// prefix the server itself reported in `LIST` and is already in the
/// server's own encoding, so encoding it again would produce a path no
/// mailbox has: for a parent «Проекты» (`&BB8EQAQ+BDUEOgRCBEs-` on the
/// wire) a second pass escapes the leading `&` and yields
/// `&-BB8EQAQ+BDUEOgRCBEs-…`.
///
/// This lives in Core, not in `feathermail-providers`, because both sides
/// need the same answer and only one of them may own it: the provider
/// sends this path in `RENAME`, and `queue::settle_folder_rename` writes
/// it into `folders.remote_id` when the server acks. Two implementations
/// that drifted by one character would leave the local row pointing at a
/// mailbox that does not exist. `providers` depends on `core` (D9), so it
/// calls this one.
pub fn mailbox_remote_id(parent_remote_id: &str, delimiter: &str, label: &str) -> String {
    let leaf = encode_modified_utf7(label);
    if delimiter.is_empty() {
        leaf
    } else {
        format!("{parent_remote_id}{delimiter}{leaf}")
    }
}

/// Encode a human mailbox label into modified UTF-7 (RFC 3501 §5.1.3) for
/// `CREATE`/`RENAME`.
///
/// Printable US-ASCII (0x20..=0x7E) goes on the wire as-is except `&`,
/// which becomes `&-`; every other run of characters is encoded as
/// UTF-16BE and then base64'd in the modified alphabet -- no padding, and
/// `,` where ordinary base64 writes `/` -- wrapped in `&`…`-`.
///
/// Only a label the user typed may be encoded. Anything that came back
/// from `LIST` (a `remote_id`) is already in the server's own encoding and
/// must go back byte-for-byte, or it would be encoded twice.
pub fn encode_modified_utf7(input: &str) -> String {
    /// RFC 3501's modified base64 alphabet: standard, with `,` for `/`.
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+,";

    fn flush(run: &mut Vec<u16>, out: &mut String) {
        if run.is_empty() {
            return;
        }
        let mut bytes = Vec::with_capacity(run.len() * 2);
        for word in run.drain(..) {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
        out.push('&');
        for chunk in bytes.chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = chunk.get(1).copied().map_or(0, u32::from);
            let b2 = chunk.get(2).copied().map_or(0, u32::from);
            let triple = (b0 << 16) | (b1 << 8) | b2;
            // 3 bytes -> 4 characters, 2 -> 3, 1 -> 2. The unused tail is
            // simply not written: this alphabet carries no `=` padding.
            for i in 0..=chunk.len() {
                let index = (triple >> (18 - 6 * i)) & 0x3f;
                out.push(ALPHABET[index as usize] as char);
            }
        }
        out.push('-');
    }

    let mut encoded = String::with_capacity(input.len());
    let mut run: Vec<u16> = Vec::new();
    for ch in input.chars() {
        match ch {
            '&' => {
                flush(&mut run, &mut encoded);
                encoded.push_str("&-");
            }
            '\u{20}'..='\u{7e}' => {
                flush(&mut run, &mut encoded);
                encoded.push(ch);
            }
            other => {
                let mut buf = [0u16; 2];
                run.extend_from_slice(other.encode_utf16(&mut buf));
            }
        }
    }
    flush(&mut run, &mut encoded);
    encoded
}

/// Minimal `"key":"value"` extractor for the flat payloads this module
/// writes with [`json_escape`] (only `\` and `"` are ever escaped). The
/// mirror image of `feathermail_providers::apply`'s private copy: same
/// payloads, opposite side of D9's boundary, and neither crate carries a
/// JSON dependency for two fields.
///
/// `pub(crate)` for `queue::settle_folder_rename`.
pub(crate) fn json_string_field(payload: &str, key: &str) -> Option<String> {
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

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// pub(crate): T-048's search.rs reuses this (and `map_thread` below) to
// turn `messages_fts`/predicate matches into the same `Thread` shape
// `list_threads`/`get_thread` return, rather than growing a second,
// possibly-drifting column list and mapper for search results.
pub(crate) const THREAD_COLUMNS: &str = "\
t.id, t.account_id, t.folder_id, t.subject, t.snippet, t.date, \
t.unread, t.starred, t.archived, t.deleted, t.has_attachment, \
t.importance, t.message_count, t.snooze_until, \
latest.sender_name, latest.sender_email, latest.recipients, latest.id";

const THREAD_MESSAGE_COLUMNS: &str = "\
m.id, m.account_id, m.thread_id, m.folder_id, m.provider_uid, \
m.message_id_header, m.date, m.sender_name, m.sender_email, \
m.subject, m.unread, m.starred, m.has_attachment, m.size_bytes";

/// One latest-message projection shared by list, open-thread, and search
/// paths. The account predicate belongs in both the correlated lookup and
/// the join: a malformed/cross-account row must never become the visible
/// sender, recipient, or body target of another account's thread.
pub(crate) const THREAD_LATEST_JOIN: &str = "\
LEFT JOIN messages latest ON latest.id = ( \
    SELECT m_latest.id FROM messages m_latest \
    WHERE m_latest.thread_id = t.id AND m_latest.account_id = t.account_id \
    ORDER BY m_latest.date DESC, m_latest.id DESC LIMIT 1 \
)";

fn folder_filter(folder: &str) -> (&'static str, bool) {
    match folder {
        "archive" => ("t.archived = 1 AND t.deleted = 0", false),
        "trash" => ("t.deleted = 1", false),
        "snoozed" => (
            "t.deleted = 0 AND t.archived = 0 AND t.snooze_until IS NOT NULL",
            false,
        ),
        "starred" => (
            "t.deleted = 0 AND t.archived = 0 AND t.snooze_until IS NULL AND t.starred = 1",
            false,
        ),
        _ => (
            "t.folder_id = ? AND t.archived = 0 AND t.deleted = 0 AND t.snooze_until IS NULL",
            true,
        ),
    }
}

/// T-108: the same question [`folder_filter`] answers, asked without an
/// account. Inbox and Sent are real rows -- one per account -- so they are
/// matched by kind through `folders`; Starred and Trash were never
/// `folder_id` predicates in the first place and read exactly as they do in
/// a single mailbox. Any other kind returns `None`: the unified mailbox
/// deliberately shows four folders (`FolderKind::UNIFIED_ORDER`), and a
/// caller asking for a fifth is asking for a view that does not exist.
pub(crate) fn unified_folder_filter(kind: FolderKind) -> Option<&'static str> {
    match kind {
        FolderKind::Inbox => Some(
            "t.folder_id IN (SELECT id FROM folders WHERE kind = 'inbox') \
             AND t.archived = 0 AND t.deleted = 0 AND t.snooze_until IS NULL",
        ),
        FolderKind::Sent => Some(
            "t.folder_id IN (SELECT id FROM folders WHERE kind = 'sent') \
             AND t.archived = 0 AND t.deleted = 0 AND t.snooze_until IS NULL",
        ),
        FolderKind::Starred => {
            Some("t.deleted = 0 AND t.archived = 0 AND t.snooze_until IS NULL AND t.starred = 1")
        }
        FolderKind::Trash => Some("t.deleted = 1"),
        _ => None,
    }
}

/// D15's folder-local pagination must not let the account-wide FTS ordering
/// index turn a concrete Inbox/Custom page into an across-folder scan. Virtual
/// folders have no `folder_id = ?` predicate, so they deliberately keep the
/// planner free to select their appropriate index.
fn thread_page_source(bind_folder: bool) -> &'static str {
    if bind_folder {
        "threads t INDEXED BY threads_account_folder_date"
    } else {
        "threads t"
    }
}

/// SQL predicate for the four fixed Filter chips from D36. This accepts a
/// closed enum rather than text, so no user-controlled value reaches SQL.
fn thread_filter_sql(filter: ThreadFilter) -> &'static str {
    match filter {
        ThreadFilter::All => "1 = 1",
        ThreadFilter::Unread => "t.unread = 1",
        ThreadFilter::Starred => "t.starred = 1",
        ThreadFilter::Attachments => "t.has_attachment = 1",
    }
}

/// The wire coordinates captured at the instant an optimistic move is
/// dispatched.  `messages.folder_id` deliberately stays on the source until
/// the provider ACKs (or source sync observes the ACKed UID vanish), so this
/// snapshot is what keeps a queued operation actionable after the thread's
/// visible folder has changed.
struct MoveTarget {
    folder_id: String,
    remote_id: String,
    messages: Vec<MoveMessage>,
}

struct MoveMessage {
    id: String,
    source_folder_id: String,
    source_remote_id: String,
    source_uid: u32,
}

/// Resolve a real destination and capture every source locator for one
/// thread. A missing destination is the explicit fallback to the pre-T-076
/// overlay/Move behavior. Once a real destination exists, a missing source
/// mailbox/UID is a conflict: falling back at that point would claim a real
/// folder while enqueueing an operation that can never be applied safely.
fn real_move_target(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    cmd: &Command,
    tid: &str,
) -> Result<Option<MoveTarget>, CoreError> {
    let destination: Option<(String, String)> = match cmd {
        Command::Archive { .. } => tx
            .query_row(
                "SELECT id, remote_id FROM folders
                 WHERE account_id = ?1 AND kind = 'archive'
                   AND remote_id IS NOT NULL AND remote_id <> ''
                 ORDER BY id LIMIT 1",
                params![account],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?,
        Command::Trash { .. } => tx
            .query_row(
                "SELECT id, remote_id FROM folders
                 WHERE account_id = ?1 AND kind = 'trash'
                   AND remote_id IS NOT NULL AND remote_id <> ''
                 ORDER BY id LIMIT 1",
                params![account],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?,
        Command::Move { folder_id, .. } => tx
            .query_row(
                "SELECT id, remote_id FROM folders
                 WHERE account_id = ?1 AND id = ?2
                   AND remote_id IS NOT NULL AND remote_id <> ''",
                params![account, folder_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?,
        _ => None,
    };
    let Some((folder_id, remote_id)) = destination else {
        return Ok(None);
    };

    let mut stmt = tx
        .prepare(
            "SELECT m.id, m.folder_id, f.remote_id, m.provider_uid
             FROM messages m JOIN folders f ON f.id = m.folder_id
             WHERE m.account_id = ?1 AND m.thread_id = ?2
             ORDER BY m.id",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![account, tid], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err)?;
    let mut messages = Vec::with_capacity(rows.len());
    for (id, source_folder_id, source_remote_id, source_uid) in rows {
        let Some(source_remote_id) = source_remote_id.filter(|v| !v.is_empty()) else {
            return Err(CoreError::new(
                ErrorCode::Conflict,
                "This message has no confirmed source mailbox yet.",
            ));
        };
        let Some(source_uid) = source_uid.and_then(|v| u32::try_from(v).ok()) else {
            return Err(CoreError::new(
                ErrorCode::Conflict,
                "This message has no confirmed source UID yet.",
            ));
        };
        messages.push(MoveMessage {
            id,
            source_folder_id,
            source_remote_id,
            source_uid,
        });
    }
    Ok(Some(MoveTarget {
        folder_id,
        remote_id,
        messages,
    }))
}

/// The wire coordinates a `PermanentDelete` is allowed to EXPUNGE, frozen
/// at dispatch (T-081 / providers-03).
///
/// `Move`/`Archive`/`Trash` capture their targets in `operation_moves`;
/// `PermanentDelete` cannot reuse that table, because the whole queue
/// treats a row there as an *active move* -- `queue::finish` rehomes the
/// message to `destination_folder_id`, `fail_and_undo` rehomes it back, and
/// `RemoteLocator::thread_messages` hides the current row in favour of the
/// captured source. So the snapshot rides in the operation's own payload
/// instead, and [`crate::locator`] reads it back for
/// `thread_messages_for_operation`.
///
/// Why a snapshot at all: EXPUNGE is irreversible (`consume_undo_in`
/// refuses to undo a `PermanentDelete`) and the queue is asynchronous. A
/// reply that lands in the thread while the operation waits -- offline, or
/// in network backoff -- would otherwise be destroyed by a command the user
/// issued before it ever arrived.
///
/// Messages with no confirmed mailbox or UID are left out, the same rule
/// `RemoteLocator::thread_messages` applies: there is no honest coordinate
/// to act on. Ordering by `messages.id` keeps the payload -- and therefore
/// D29's idempotency hash -- stable for the same set of targets.
fn permanent_delete_payload(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    tid: &str,
) -> Result<String, CoreError> {
    let mut stmt = tx
        .prepare(
            "SELECT f.remote_id, m.provider_uid
             FROM messages m JOIN folders f ON f.id = m.folder_id
             WHERE m.account_id = ?1 AND m.thread_id = ?2
             ORDER BY m.id",
        )
        .map_err(sql_err)?;
    let rows = stmt
        .query_map(params![account, tid], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<i64>>(1)?,
            ))
        })
        .map_err(sql_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(sql_err)?;
    let targets = rows
        .into_iter()
        .filter_map(|(remote_id, uid)| {
            let folder = remote_id.filter(|s| !s.is_empty())?;
            let uid = uid.filter(|v| *v >= 0)?;
            Some(format!(
                r#"{{"folder":"{}","uid":{uid}}}"#,
                json_escape(&folder)
            ))
        })
        .collect::<Vec<_>>();
    Ok(format!(r#"{{"targets":[{}]}}"#, targets.join(",")))
}

struct UndoOperation {
    id: OperationId,
    account_id: String,
    target_id: String,
    kind: OpKind,
    status: crate::model::OpStatus,
    undo_payload: Option<String>,
}

type MoveCoordinate = (String, String, String, i64, String, String, Option<i64>);

impl UndoOperation {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let kind: String = row.get(3)?;
        let kind = kind.parse().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unknown op kind",
                )),
            )
        })?;
        let status: String = row.get(4)?;
        let status = status.parse().map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "unknown op status",
                )),
            )
        })?;
        Ok(Self {
            id: OperationId(row.get(0)?),
            account_id: row.get(1)?,
            target_id: row.get(2)?,
            kind,
            status,
            undo_payload: row.get(5)?,
        })
    }
}

/// The one definition of "this thread is no longer snoozed": back to Inbox,
/// the local snooze ledger row cancelled, the snooze deleted.
///
/// Shared by the scheduler ([`Core::wake_due_snoozes`]) and by an explicit
/// [`Core::unsnooze_thread`] so the two can never drift apart. Snooze is a
/// local overlay (D26), so this queues nothing for IMAP.
fn wake_snooze_in(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
    thread_id: &str,
) -> Result<(), CoreError> {
    tx.execute(
        "UPDATE threads
         SET folder_id = COALESCE(
                 (SELECT id FROM folders
                  WHERE account_id = ?1 AND kind = 'inbox'
                  ORDER BY id LIMIT 1),
                 folder_id
             ),
             archived = 0, deleted = 0, snooze_until = NULL
         WHERE account_id = ?1 AND id = ?2",
        params![account_id, thread_id],
    )
    .map_err(sql_err)?;
    tx.execute(
        "UPDATE operations SET status = 'cancelled', next_attempt_at = NULL
         WHERE account_id = ?1 AND target_id = ?2
           AND op = 'snooze' AND status = 'local'",
        params![account_id, thread_id],
    )
    .map_err(sql_err)?;
    tx.execute(
        "DELETE FROM snoozes WHERE account_id = ?1 AND thread_id = ?2",
        params![account_id, thread_id],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Apply the durable Undo state machine within a caller-owned transaction.
/// Keeping it separate from commit/event delivery lets restore select its
/// eligible Trash lifecycle and consume it at the same SQLite linearization
/// point as the ordinary Undo ticket path.
fn consume_undo_in(
    tx: &rusqlite::Transaction<'_>,
    original: &UndoOperation,
    requested_at: i64,
) -> Result<(UndoReceipt, MailEvent), CoreError> {
    if original.kind == OpKind::PermanentDelete {
        return Err(CoreError::from_code(ErrorCode::OperationNotSupported));
    }
    let event = || MailEvent::ThreadsChanged {
        account_id: AccountId(original.account_id.clone()),
        thread_ids: vec![ThreadId(original.target_id.clone())],
    };

    // T-035/D26: Snooze is a local overlay, so Undo must never create a
    // second queue row (and must never ask IMAP to do anything). Its ledger
    // row is `local`, not `acked`; cancel it in place while restoring the
    // exact pre-snooze snapshot.
    if original.kind == OpKind::Snooze {
        if !matches!(
            original.status,
            crate::model::OpStatus::Pending
                | crate::model::OpStatus::Running
                | crate::model::OpStatus::Acked
                | crate::model::OpStatus::Local
        ) {
            return Err(CoreError::from_code(ErrorCode::OperationNotSupported));
        }
        let previous_folder_id: Option<String> = tx
            .query_row(
                "SELECT previous_folder_id FROM snoozes
                 WHERE account_id = ?1 AND thread_id = ?2",
                params![original.account_id, original.target_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let current_folder_id = read_thread_fields(tx, &original.account_id, &original.target_id)?
            .ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))?
            .folder_id;
        let previous_until = original
            .undo_payload
            .as_deref()
            .and_then(|snapshot| json_opt_i64_field(snapshot, "snooze_until"))
            .flatten();
        if let Some(snapshot) = original.undo_payload.as_deref() {
            apply_undo(tx, &original.account_id, &original.target_id, snapshot)?;
        }
        if let Some(until) = previous_until {
            let previous_folder_id = previous_folder_id.unwrap_or(current_folder_id);
            tx.execute(
                "INSERT INTO snoozes
                 (id, account_id, thread_id, until_ts, previous_folder_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(account_id, thread_id) DO UPDATE SET
                   until_ts = excluded.until_ts,
                   previous_folder_id = excluded.previous_folder_id",
                params![
                    format!("snooze:{}:{}", original.account_id, original.target_id),
                    original.account_id,
                    original.target_id,
                    until,
                    previous_folder_id,
                ],
            )
            .map_err(sql_err)?;
        } else {
            tx.execute(
                "DELETE FROM snoozes WHERE account_id = ?1 AND thread_id = ?2",
                params![original.account_id, original.target_id],
            )
            .map_err(sql_err)?;
        }
        tx.execute(
            "UPDATE operations SET status = 'cancelled', next_attempt_at = NULL,
                 undo_requested_at = ?2 WHERE id = ?1",
            params![original.id.as_str(), requested_at],
        )
        .map_err(sql_err)?;
        return Ok((
            UndoReceipt::Cancelled {
                operation_id: original.id.clone(),
            },
            event(),
        ));
    }

    match original.status {
        crate::model::OpStatus::Pending => {
            if let Some(snapshot) = original.undo_payload.as_deref() {
                apply_undo(tx, &original.account_id, &original.target_id, snapshot)?;
            }
            archive_move_history(tx, original.id.as_str())?;
            tx.execute(
                "UPDATE operations SET status = 'cancelled', next_attempt_at = NULL,
                 undo_requested_at = ?2
                 WHERE id = ?1 AND status = 'pending'",
                params![original.id.as_str(), requested_at],
            )
            .map_err(sql_err)?;
            tx.execute(
                "DELETE FROM operation_moves WHERE operation_id = ?1",
                params![original.id.as_str()],
            )
            .map_err(sql_err)?;
            Ok((
                UndoReceipt::Cancelled {
                    operation_id: original.id.clone(),
                },
                event(),
            ))
        }
        crate::model::OpStatus::Running | crate::model::OpStatus::Acked => {
            let before_reverse = read_thread_fields(tx, &original.account_id, &original.target_id)?
                .ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))?;
            if let Some(snapshot) = original.undo_payload.as_deref() {
                apply_undo(tx, &original.account_id, &original.target_id, snapshot)?;
            }
            tx.execute(
                "UPDATE operations SET undo_requested_at = ?2 WHERE id = ?1",
                params![original.id.as_str(), requested_at],
            )
            .map_err(sql_err)?;
            let reverse = materialize_reverse(tx, original, &before_reverse)?;
            Ok((
                UndoReceipt::ReverseQueued {
                    operation_id: original.id.clone(),
                    reverse_operation_id: reverse,
                },
                event(),
            ))
        }
        crate::model::OpStatus::Failed
        | crate::model::OpStatus::Blocked
        | crate::model::OpStatus::Cancelled
        | crate::model::OpStatus::Local => {
            Err(CoreError::from_code(ErrorCode::OperationNotSupported))
        }
    }
}

/// Copy active coordinates into the append-only history before an active
/// intent is removed. The operation row remains forever, so the history has
/// no foreign key by design: it is an audit/recovery record, not live queue
/// state.
pub(crate) fn archive_move_history(
    tx: &rusqlite::Transaction<'_>,
    operation_id: &str,
) -> Result<(), CoreError> {
    tx.execute(
        "INSERT OR IGNORE INTO operation_move_history
         (operation_id, message_id, source_folder_id, source_remote_id,
          source_uid, destination_folder_id, destination_remote_id,
          destination_uid, recorded_at)
         SELECT operation_id, message_id, source_folder_id, source_remote_id,
                source_uid, destination_folder_id, destination_remote_id,
                destination_uid, strftime('%s','now')
         FROM operation_moves
         WHERE operation_id = ?1 AND destination_uid IS NOT NULL",
        params![operation_id],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn inverse_kind(kind: OpKind) -> Option<(OpKind, &'static str)> {
    match kind {
        OpKind::Archive | OpKind::Trash | OpKind::Move => Some((OpKind::Move, "move")),
        OpKind::MarkRead => Some((OpKind::MarkUnread, "mark_unread")),
        OpKind::MarkUnread => Some((OpKind::MarkRead, "mark_read")),
        OpKind::Star => Some((OpKind::Unstar, "unstar")),
        OpKind::Unstar => Some((OpKind::Star, "star")),
        // Snooze has no provider representation; a reverse local operation
        // is still useful for the durable lifecycle and ACKs as a no-op.
        OpKind::Snooze => Some((OpKind::Snooze, "snooze")),
        // Folder-shaped operations have no thread-shaped inverse: undoing a
        // rename is another rename, which the user issues themselves, and a
        // deleted mailbox cannot be un-deleted on the wire at all.
        OpKind::PermanentDelete
        | OpKind::Send
        | OpKind::SyncDraft
        | OpKind::CreateFolder
        | OpKind::RenameFolder
        | OpKind::DeleteFolder => None,
    }
}

fn materialize_reverse(
    tx: &rusqlite::Transaction<'_>,
    original: &UndoOperation,
    before_reverse: &ThreadFields,
) -> Result<OperationId, CoreError> {
    let reverse_id = OperationId(format!("undo:{}", original.id.as_str()));
    if let Some(existing) = tx
        .query_row(
            "SELECT id FROM operations WHERE undo_of = ?1",
            params![original.id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(sql_err)?
    {
        return Ok(OperationId(existing));
    }
    let Some((reverse_kind, kind_name)) = inverse_kind(original.kind) else {
        return Err(CoreError::from_code(ErrorCode::OperationNotSupported));
    };

    let move_rows: Vec<MoveCoordinate> = {
        let mut stmt = tx
            .prepare(
                "SELECT message_id, source_folder_id, source_remote_id, source_uid,
                        destination_folder_id, destination_remote_id, destination_uid
                 FROM operation_moves WHERE operation_id = ?1
                 UNION ALL
                 SELECT message_id, source_folder_id, source_remote_id, source_uid,
                        destination_folder_id, destination_remote_id, destination_uid
                 FROM operation_move_history WHERE operation_id = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM operation_moves active
                       WHERE active.operation_id = operation_move_history.operation_id
                         AND active.message_id = operation_move_history.message_id
                   )
                 ORDER BY message_id",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![original.id.as_str()], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        rows
    };
    let has_unresolved_move = move_rows.iter().any(|row| row.6.is_none());
    let blocked = matches!(original.status, crate::model::OpStatus::Running) || has_unresolved_move;

    let payload = if reverse_kind == OpKind::Move {
        let folder = original
            .undo_payload
            .as_deref()
            .and_then(|v| json_field_string(v, "folder_id"))
            .unwrap_or_else(|| before_reverse.folder_id.clone());
        format!(r#"{{"folder_id":"{}"}}"#, json_escape(&folder))
    } else if reverse_kind == OpKind::MarkRead {
        r#"{"read":true}"#.to_string()
    } else if reverse_kind == OpKind::MarkUnread {
        r#"{"read":false}"#.to_string()
    } else if reverse_kind == OpKind::Star {
        r#"{"starred":true}"#.to_string()
    } else if reverse_kind == OpKind::Unstar {
        r#"{"starred":false}"#.to_string()
    } else {
        // Provider treats Snooze as local-only; retain a valid payload so
        // the reverse is still idempotent and auditable.
        r#"{"until":0}"#.to_string()
    };
    let reverse_undo = undo_snapshot(before_reverse, reverse_kind, reverse_kind == OpKind::Move);
    let hash = payload_hash(&payload);
    tx.execute(
        "INSERT INTO operations
         (id, account_id, target_id, op, payload, payload_hash, created_at,
          retry_count, status, undo_payload, undo_of, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'), 0, ?7, ?8, ?9,
                 (SELECT COALESCE(MAX(seq), 0) + 1 FROM operations))",
        params![
            reverse_id.as_str(),
            original.account_id,
            original.target_id,
            kind_name,
            payload,
            hash,
            if blocked { "blocked" } else { "pending" },
            reverse_undo,
            original.id.as_str(),
        ],
    )
    .map_err(sql_err)?;

    if reverse_kind == OpKind::Move && !move_rows.is_empty() && !has_unresolved_move {
        for (
            message_id,
            source_folder_id,
            source_remote_id,
            _source_uid,
            destination_folder_id,
            destination_remote_id,
            destination_uid,
        ) in move_rows
        {
            tx.execute(
                "INSERT INTO operation_moves
                 (operation_id, message_id, source_folder_id, source_remote_id,
                  source_uid, destination_folder_id, destination_remote_id,
                  destination_uid)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
                params![
                    reverse_id.as_str(),
                    message_id,
                    destination_folder_id,
                    destination_remote_id,
                    destination_uid,
                    source_folder_id,
                    source_remote_id,
                ],
            )
            .map_err(sql_err)?;
        }
    }
    Ok(reverse_id)
}

/// Executes Core's vector command loop inside an already-open transaction.
/// The public dispatcher commits and emits after this returns; specialised
/// high-risk doors can instead compose the exact same loop with their
/// approval consume before making either effect visible.
pub(crate) fn dispatch_with_receipt_in(
    tx: &rusqlite::Transaction<'_>,
    cmd: &Command,
    now: i64,
) -> Result<DispatchReceipt, CoreError> {
    let ids = cmd.thread_ids();
    if ids.is_empty() {
        return Err(CoreError::from_code(ErrorCode::InvalidArgument));
    }
    let mut operations = Vec::new();
    for id in &ids {
        let (n, operation_id) = apply_one(tx, cmd, id, now)?;
        if n == 0 {
            return Err(CoreError::from_code(ErrorCode::MessageNotFound));
        }
        if let Some(operation_id) = operation_id {
            operations.push(OperationReceipt {
                thread_id: id.clone(),
                operation_id,
            });
        }
    }
    Ok(DispatchReceipt { operations })
}

fn apply_one(
    tx: &rusqlite::Transaction<'_>,
    cmd: &Command,
    id: &ThreadId,
    now: i64,
) -> Result<(usize, Option<OperationId>), CoreError> {
    let account = cmd.account_id().as_str();
    let tid = id.as_str();
    // A `Move` names its destination by raw `folders.id`, and the FK on
    // `threads.folder_id` only proves the row exists -- not that it belongs
    // to this account. Without this check a thread of account A could be
    // parked in a folder of account B: gone from A's sidebar (no such
    // folder) and filtered out of B's list (`t.account_id`), i.e. invisible
    // everywhere. Refuse before any UPDATE so a rejected thread cannot leave
    // half of a batch applied. `deleted_at IS NULL` because a tombstone is
    // not in `list_folders` either, so landing there hides the thread the
    // same way; `revive_folder` clears the column when the folder comes back.
    if let Command::Move { folder_id, .. } = cmd {
        let known = tx
            .query_row(
                "SELECT 1 FROM folders
                 WHERE account_id = ?1 AND id = ?2 AND deleted_at IS NULL",
                params![account, folder_id.as_str()],
                |_| Ok(()),
            )
            .optional()
            .map_err(sql_err)?
            .is_some();
        if !known {
            return Err(CoreError::new(
                ErrorCode::InvalidArgument,
                "That folder isn't in this account.",
            ));
        }
    }
    let Some(before) = read_thread_fields(tx, account, tid)? else {
        return Ok((0, None));
    };
    let real_move = match cmd {
        Command::Archive { .. } | Command::Trash { .. } | Command::Move { .. } => {
            real_move_target(tx, account, cmd, tid)?
        }
        _ => None,
    };
    let moving_to_real_folder = real_move.is_some();
    let same_real_folder = real_move.as_ref().is_some_and(|target| {
        !target.messages.is_empty()
            && target
                .messages
                .iter()
                .all(|message| message.source_folder_id == target.folder_id)
    });
    let (kind, sql, payload) = match cmd {
        Command::Archive { .. } if moving_to_real_folder => (
            OpKind::Archive,
            "UPDATE threads SET folder_id = ?3, archived = 0, unread = 0, deleted = 0, snooze_until = NULL \
             WHERE account_id = ?1 AND id = ?2",
            "{}",
        ),
        Command::Trash { .. } if moving_to_real_folder => (
            OpKind::Trash,
            "UPDATE threads SET folder_id = ?3, deleted = 0, unread = 0, archived = 0, snooze_until = NULL \
             WHERE account_id = ?1 AND id = ?2",
            "{}",
        ),
        Command::Archive { .. } => (
            OpKind::Archive,
            "UPDATE threads SET archived = 1, unread = 0, deleted = 0, snooze_until = NULL \
             WHERE account_id = ?1 AND id = ?2",
            "{}",
        ),
        Command::Trash { .. } => (
            OpKind::Trash,
            "UPDATE threads SET deleted = 1, unread = 0, snooze_until = NULL \
             WHERE account_id = ?1 AND id = ?2",
            "{}",
        ),
        Command::PermanentDelete { .. } => (
            OpKind::PermanentDelete,
            "UPDATE threads SET deleted = 1, unread = 0, snooze_until = NULL \
             WHERE account_id = ?1 AND id = ?2",
            "{}",
        ),
        Command::MarkRead { .. } => (
            OpKind::MarkRead,
            "UPDATE threads SET unread = 0 WHERE account_id = ?1 AND id = ?2 AND deleted = 0",
            r#"{"read":true}"#,
        ),
        Command::MarkUnread { .. } => (
            OpKind::MarkUnread,
            "UPDATE threads SET unread = 1 WHERE account_id = ?1 AND id = ?2 AND deleted = 0",
            r#"{"read":false}"#,
        ),
        Command::Star { .. } => (
            OpKind::Star,
            "UPDATE threads SET starred = 1 WHERE account_id = ?1 AND id = ?2",
            r#"{"starred":true}"#,
        ),
        Command::Unstar { .. } => (
            OpKind::Unstar,
            "UPDATE threads SET starred = 0 WHERE account_id = ?1 AND id = ?2",
            r#"{"starred":false}"#,
        ),
        Command::Snooze { .. } => (
            OpKind::Snooze,
            "UPDATE threads SET snooze_until = ?3, archived = 0, deleted = 0 \
             WHERE account_id = ?1 AND id = ?2",
            "",
        ),
        Command::Move { .. } if moving_to_real_folder => (
            OpKind::Move,
            "UPDATE threads SET folder_id = ?3, archived = 0, deleted = 0, snooze_until = NULL \
             WHERE account_id = ?1 AND id = ?2",
            "",
        ),
        Command::Move { .. } => (
            OpKind::Move,
            "UPDATE threads SET folder_id = ?3, archived = 0, deleted = 0, snooze_until = NULL \
             WHERE account_id = ?1 AND id = ?2",
            "",
        ),
    };
    let n = match cmd {
        Command::Snooze { until, .. } => tx
            .execute(sql, params![account, tid, until])
            .map_err(sql_err)?,
        Command::Move { folder_id, .. } => tx
            .execute(
                sql,
                params![
                    account,
                    tid,
                    real_move
                        .as_ref()
                        .map(|target| target.folder_id.as_str())
                        .unwrap_or(folder_id.as_str())
                ],
            )
            .map_err(sql_err)?,
        Command::Archive { .. } | Command::Trash { .. } if moving_to_real_folder => tx
            .execute(
                sql,
                params![account, tid, real_move.as_ref().unwrap().folder_id.as_str()],
            )
            .map_err(sql_err)?,
        _ => tx.execute(sql, params![account, tid]).map_err(sql_err)?,
    };
    if let Command::Snooze { until, .. } = cmd {
        // Keep the local deadline durable across restart. The thread column
        // remains the hot list/query projection; this row is the scheduler's
        // source of truth and records where the thread was before the
        // overlay was applied.
        tx.execute(
            "INSERT INTO snoozes (id, account_id, thread_id, until_ts, previous_folder_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account_id, thread_id) DO UPDATE SET
                 until_ts = excluded.until_ts,
                 previous_folder_id = excluded.previous_folder_id",
            params![
                format!("snooze:{account}:{tid}"),
                account,
                tid,
                until,
                before.folder_id,
            ],
        )
        .map_err(sql_err)?;
    }
    // T-029: `rollup_folder` reads MAX(messages.unread/starred). A dispatch
    // that only touched `threads` was clobbered by the next CONDSTORE
    // upsert. Write members even when `n == 0` (thread already in the
    // requested state): a 1:1 profile can have `threads.unread = 0` from
    // T-028 and stale `messages.unread = 1` from IMAP until rethread
    // copy-down. Skipping here would leave MAX(messages) able to resurrect
    // unread on the next grouping.
    apply_member_flags(tx, cmd, account, tid)?;
    // D26/T-035: Snooze is a local overlay. Keep its durable Undo ticket in
    // `operations`, but mark it `local`, a status that `queue::claim_next`
    // never selects. This gives Core a restart-safe ticket without making a
    // provider apply a command IMAP cannot represent or fabricating a wire
    // ACK for a change that never reached the server.
    if let Command::Snooze { until, .. } = cmd {
        let payload = format!(r#"{{"until":{until}}}"#);
        let undo = undo_snapshot(&before, kind, moving_to_real_folder);
        tx.execute(
            "UPDATE operations SET status = 'cancelled', next_attempt_at = NULL
             WHERE account_id = ?1 AND target_id = ?2
               AND op = 'snooze' AND status = 'local'",
            params![account, tid],
        )
        .map_err(sql_err)?;
        let operation_id = enqueue(tx, account, tid, kind, &payload, Some(&undo), now)?;
        tx.execute(
            "UPDATE operations SET status = 'local', next_attempt_at = NULL
             WHERE id = ?1",
            params![operation_id.as_str()],
        )
        .map_err(sql_err)?;
        return Ok((n, Some(operation_id)));
    }
    // A real Move whose every source row is already in the destination has
    // nothing to send and, crucially, must not leave a durable intent waiting
    // for a destination UID that can never change.  Keep the local command's
    // flag update above, but finish it as a local no-op.
    if same_real_folder {
        return Ok((1, None));
    }
    // A permanent delete must still reach the provider when the thread is
    // already in the local Trash overlay: the remote message can still be
    // present there and needs an explicit EXPUNGE. Other commands retain
    // their idempotent no-op behavior.
    if n == 0 && !matches!(cmd, Command::PermanentDelete { .. }) {
        // Already in the requested state, or the flag is unrepresentable (unread+trash).
        return Ok((1, None));
    }
    let payload = match cmd {
        Command::Move { folder_id, .. } => {
            // `folder_id` is `{account}:{slug}` and the slug keeps whatever
            // the user typed, `\` and `"` included, so it has to be escaped
            // like every other payload this module writes (`materialize_reverse`,
            // `create_folder_payload`, ...). Unescaped, the provider's
            // `json_string` reader silently returns a *different* folder id.
            format!(r#"{{"folder_id":"{}"}}"#, json_escape(folder_id.as_str()))
        }
        Command::PermanentDelete { .. } => permanent_delete_payload(tx, account, tid)?,
        _ => payload.to_string(),
    };
    let undo = undo_snapshot(&before, kind, moving_to_real_folder);
    let operation_id = enqueue(tx, account, tid, kind, &payload, Some(&undo), now)?;
    if let Some(target) = real_move {
        // Keep the source/destination scheduler clocks intact until the
        // provider has actually ACKed this operation.  Resetting them here
        // makes a lost provider response look like a confirmed move: an
        // eager destination sync can see an empty mailbox, while source
        // `remove_vanished` then reaps the pending row before the retry gets
        // a chance to apply the move.  `queue::finish(Acked)` resets both
        // sides atomically after wire success.
        for message in target.messages {
            tx.execute(
                "INSERT OR IGNORE INTO operation_moves
                 (operation_id, message_id, source_folder_id, source_remote_id, source_uid,
                  destination_folder_id, destination_remote_id, destination_uid)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
                params![
                    operation_id.as_str(),
                    message.id,
                    message.source_folder_id,
                    message.source_remote_id,
                    i64::from(message.source_uid),
                    target.folder_id,
                    target.remote_id,
                ],
            )
            .map_err(sql_err)?;
        }
    }
    Ok((n, Some(operation_id)))
}

/// Mirror the thread-level unread/starred change onto every `messages` row
/// of this thread (T-029). Archive/Trash already zero `threads.unread`;
/// members get the same so a later rollup cannot resurrect unread.
fn apply_member_flags(
    tx: &rusqlite::Transaction<'_>,
    cmd: &Command,
    account: &str,
    tid: &str,
) -> Result<(), CoreError> {
    match cmd {
        Command::Archive { .. }
        | Command::Trash { .. }
        | Command::PermanentDelete { .. }
        | Command::MarkRead { .. } => {
            tx.execute(
                "UPDATE messages SET unread = 0 WHERE account_id = ?1 AND thread_id = ?2",
                params![account, tid],
            )
            .map_err(sql_err)?;
        }
        Command::MarkUnread { .. } => {
            tx.execute(
                "UPDATE messages SET unread = 1 WHERE account_id = ?1 AND thread_id = ?2",
                params![account, tid],
            )
            .map_err(sql_err)?;
        }
        Command::Star { .. } => {
            tx.execute(
                "UPDATE messages SET starred = 1 WHERE account_id = ?1 AND thread_id = ?2",
                params![account, tid],
            )
            .map_err(sql_err)?;
        }
        Command::Unstar { .. } => {
            tx.execute(
                "UPDATE messages SET starred = 0 WHERE account_id = ?1 AND thread_id = ?2",
                params![account, tid],
            )
            .map_err(sql_err)?;
        }
        Command::Snooze { .. } | Command::Move { .. } => {}
    }
    Ok(())
}

/// The `threads` columns [`apply_one`] reads just before running a
/// command's own `UPDATE` (T-081), so a later rollback knows what to put
/// back. `i64` for the boolean-flag columns matches how SQLite (and the
/// rest of this module) already stores them.
struct ThreadFields {
    unread: i64,
    starred: i64,
    archived: i64,
    deleted: i64,
    snooze_until: Option<i64>,
    folder_id: String,
}

fn read_thread_fields(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    tid: &str,
) -> Result<Option<ThreadFields>, CoreError> {
    tx.query_row(
        "SELECT unread, starred, archived, deleted, snooze_until, folder_id
         FROM threads WHERE account_id = ?1 AND id = ?2",
        params![account, tid],
        |row| {
            Ok(ThreadFields {
                unread: row.get(0)?,
                starred: row.get(1)?,
                archived: row.get(2)?,
                deleted: row.get(3)?,
                snooze_until: row.get(4)?,
                folder_id: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(sql_err)
}

/// `operations.undo_payload` for one queued command (T-081): the pre-
/// mutation value of exactly the `threads` columns `kind`'s own `UPDATE`
/// (the `sql` match arm above) is about to overwrite -- no more. Scoped to
/// just those columns rather than a full-row snapshot on purpose: a
/// full-row restore issued later, when this operation's provider apply
/// finally fails, could stomp on a column a *different*, successfully
/// applied operation changed on the same thread in the meantime (e.g. a
/// `Move` that landed while this `Archive` was still queued) -- rolling
/// back only the columns this command actually touched can't do that.
///
/// Deliberately not part of `operations.payload`: `payload_hash` is D29's
/// idempotency key, and folding the *previous* state into it would make
/// two `Archive` calls on the same thread from different starting states
/// hash to two different operations, breaking dedup.
fn undo_snapshot(before: &ThreadFields, kind: OpKind, moved_to_real_folder: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    match kind {
        OpKind::Archive => {
            parts.push(format!("\"unread\":{}", before.unread));
            parts.push(format!("\"archived\":{}", before.archived));
            parts.push(format!("\"deleted\":{}", before.deleted));
            parts.push(format!(
                "\"snooze_until\":{}",
                json_opt_i64(before.snooze_until)
            ));
            if moved_to_real_folder {
                parts.push(format!(
                    "\"folder_id\":\"{}\"",
                    json_escape(&before.folder_id)
                ));
            }
        }
        OpKind::Trash => {
            parts.push(format!("\"unread\":{}", before.unread));
            parts.push(format!("\"deleted\":{}", before.deleted));
            if moved_to_real_folder {
                parts.push(format!("\"archived\":{}", before.archived));
            }
            parts.push(format!(
                "\"snooze_until\":{}",
                json_opt_i64(before.snooze_until)
            ));
            if moved_to_real_folder {
                parts.push(format!(
                    "\"folder_id\":\"{}\"",
                    json_escape(&before.folder_id)
                ));
            }
        }
        OpKind::PermanentDelete => {
            parts.push(format!("\"unread\":{}", before.unread));
            parts.push(format!("\"deleted\":{}", before.deleted));
            parts.push(format!(
                "\"snooze_until\":{}",
                json_opt_i64(before.snooze_until)
            ));
        }
        OpKind::MarkRead | OpKind::MarkUnread => {
            parts.push(format!("\"unread\":{}", before.unread));
        }
        OpKind::Star | OpKind::Unstar => {
            parts.push(format!("\"starred\":{}", before.starred));
        }
        OpKind::Snooze => {
            parts.push(format!("\"archived\":{}", before.archived));
            parts.push(format!("\"deleted\":{}", before.deleted));
            parts.push(format!(
                "\"snooze_until\":{}",
                json_opt_i64(before.snooze_until)
            ));
        }
        OpKind::Move => {
            parts.push(format!("\"archived\":{}", before.archived));
            parts.push(format!("\"deleted\":{}", before.deleted));
            parts.push(format!(
                "\"snooze_until\":{}",
                json_opt_i64(before.snooze_until)
            ));
            parts.push(format!(
                "\"folder_id\":\"{}\"",
                json_escape(&before.folder_id)
            ));
        }
        // T-081 undo only covers the eight thread-mutating `Command`
        // variants `apply_one` handles; `Send`/`SyncDraft`/`CreateFolder`/
        // `RenameFolder` operations are enqueued elsewhere (`Core::send`,
        // `Core::create_folder`, `Core::rename_folder`) and never reach this
        // function.
        OpKind::Send
        | OpKind::SyncDraft
        | OpKind::CreateFolder
        | OpKind::RenameFolder
        | OpKind::DeleteFolder => {}
    }
    format!("{{{}}}", parts.join(","))
}

fn json_opt_i64(v: Option<i64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

/// Bare-token (integer or `null`) lookup in the small flat JSON strings
/// this module reads/writes on `operations.payload`/`undo_payload`
/// (T-081). Same scope note as `json_field_string` below: not a general
/// parser, just enough for the shapes `undo_snapshot` writes.
fn json_token<'a>(payload: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let start = payload.find(&needle)? + needle.len();
    let rest = &payload[start..];
    let end = rest.find([',', '}'])?;
    Some(rest[..end].trim())
}

fn json_int(payload: &str, key: &str) -> Option<i64> {
    json_token(payload, key)?.parse().ok()
}

/// `Some(None)` means the key is present and `null` (T-081's
/// `snooze_until` when the pre-mutation thread wasn't snoozed); `None`
/// means the key is absent, i.e. this operation's kind never touched that
/// column and [`apply_undo`] must leave it alone. Callers must tell those
/// two apart -- collapsing them would either skip restoring a real `NULL`
/// or clobber a column this operation never mutated.
fn json_opt_i64_field(payload: &str, key: &str) -> Option<Option<i64>> {
    let raw = json_token(payload, key)?;
    if raw == "null" {
        Some(None)
    } else {
        raw.parse::<i64>().ok().map(Some)
    }
}

/// `"key":"value"` extractor for the same flat JSON shape
/// `feathermail_providers::apply::json_string` reads -- an independent
/// copy, not a shared one, since `feathermail-core` has no dependency on
/// that crate (D9: the dependency runs the other way).
fn json_field_string(payload: &str, key: &str) -> Option<String> {
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

/// Put back exactly the `threads` columns `undo_json` names (T-081) --
/// [`crate::queue::Core::tick`]'s response to a non-retryable provider
/// failure. Column-scoped both in how `undo_snapshot` built the JSON and
/// here in how it's read back: a key simply absent from `undo_json` is
/// left untouched in `threads`, never defaulted or guessed.
///
/// When `unread` / `starred` are present, the same thread-level value is
/// written onto every `messages` row of this thread (T-029). Undo payload
/// stays one snapshot per thread, not per member — same as T-081.
pub(crate) fn apply_undo(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    tid: &str,
    undo_json: &str,
) -> Result<(), CoreError> {
    let unread = json_int(undo_json, "unread");
    let starred = json_int(undo_json, "starred");
    let mut sets: Vec<&'static str> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(v) = unread {
        sets.push("unread = ?");
        binds.push(Box::new(v));
    }
    if let Some(v) = starred {
        sets.push("starred = ?");
        binds.push(Box::new(v));
    }
    if let Some(v) = json_int(undo_json, "archived") {
        sets.push("archived = ?");
        binds.push(Box::new(v));
    }
    if let Some(v) = json_int(undo_json, "deleted") {
        sets.push("deleted = ?");
        binds.push(Box::new(v));
    }
    if let Some(v) = json_opt_i64_field(undo_json, "snooze_until") {
        sets.push("snooze_until = ?");
        binds.push(Box::new(v));
    }
    if let Some(v) = json_field_string(undo_json, "folder_id") {
        sets.push("folder_id = ?");
        binds.push(Box::new(v));
    }
    if sets.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "UPDATE threads SET {} WHERE account_id = ? AND id = ?",
        sets.join(", ")
    );
    binds.push(Box::new(account.to_string()));
    binds.push(Box::new(tid.to_string()));
    let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    tx.execute(&sql, refs.as_slice()).map_err(sql_err)?;
    if let Some(v) = unread {
        tx.execute(
            "UPDATE messages SET unread = ?1 WHERE account_id = ?2 AND thread_id = ?3",
            params![v, account, tid],
        )
        .map_err(sql_err)?;
    }
    if let Some(v) = starred {
        tx.execute(
            "UPDATE messages SET starred = ?1 WHERE account_id = ?2 AND thread_id = ?3",
            params![v, account, tid],
        )
        .map_err(sql_err)?;
    }
    Ok(())
}

fn enqueue(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    target: &str,
    kind: OpKind,
    payload: &str,
    undo_payload: Option<&str>,
    now: i64,
) -> Result<OperationId, CoreError> {
    let hash = payload_hash(payload);
    let id = format!("{}:{account}:{target}:{hash}", kind.as_str());
    // T-162: `seq` is the queue's own order of issue, handed out here and
    // nowhere else. `created_at` is whole seconds and ties constantly, and
    // the `rowid` that used to break those ties is fixed at first INSERT --
    // which is wrong for the revive below, where the row becomes claimable
    // again long after it was first written. `MAX(seq) + 1` is evaluated
    // inside this transaction, so two commands issued in the same second
    // keep the order the user issued them in.
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO operations
             (id, account_id, target_id, op, payload, payload_hash, created_at, retry_count, status, undo_payload, seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 'pending', ?8,
                     (SELECT COALESCE(MAX(seq), 0) + 1 FROM operations))",
            params![id, account, target, kind.as_str(), payload, hash, now, undo_payload],
        )
        .map_err(sql_err)?;
    if inserted == 0 {
        // T-081: the id is deterministic (kind+account+target+payload
        // hash -- D29's idempotency key), so a repeat of the same command
        // always lands on this same row, including one already `failed`
        // from an earlier non-retryable provider error. `apply_one` has
        // already redone the local mark by this point; if a `failed` row
        // were left alone here, that mark would sit in SQLite forever with
        // no operation left to reconcile it against the server -- exactly
        // the bug this task fixes, just permanent instead of one retry
        // away. Revive it: back to `pending`, `retry_count`/
        // `next_attempt_at` reset, and `undo_payload` refreshed to *this*
        // attempt's pre-mutation snapshot (the old one describes a state
        // this same call's rollback already restored, so it's stale).
        //
        // `acked` rows are revived for the same reason. D29 dedups *unsent*
        // work; it does not say a state the user legitimately returned to may
        // never be sent again. Because the id is only (kind, account, target,
        // payload hash), a thread that leaves a state and comes back to it
        // (Star -> Unstar -> Star, MarkRead -> MarkUnread -> MarkRead, Move
        // A -> B -> A) lands on the row the first occurrence already ACKed.
        // Leaving that alone dropped the third command silently: the local
        // mark stayed, no operation existed to carry it to the server, and
        // the next CONDSTORE pass overwrote the flag with the server's.
        // Re-sending a confirmed operation is the cheaper failure: every op
        // this path can revive is idempotent on the wire (STORE of a flag
        // already set, MOVE of a UID that is already gone answers
        // NotFound/Conflict, which `queue::tick` treats as success).
        //
        // `pending`/`running` rows are left alone -- landing on the same id
        // there is D29's dedup working as intended, not this bug. `local`
        // and `cancelled` are left alone deliberately: `local` is the Snooze
        // overlay (D26/T-035) that `claim_next` must never select, and
        // reviving it would put a command IMAP cannot represent on the wire.
        //
        // T-162: a revived row is *newly* claimable, so it takes a fresh
        // `seq` as well. Keeping the old one put it back in the queue at
        // the position it held when it was first issued -- ahead of every
        // command the user gave since, which for a Star revived behind a
        // Move meant a flag STORE against a UID the MOVE had already taken
        // away. The row is the same row (D29's key), the order of issue is
        // not.
        tx.execute(
            "UPDATE operations
             SET status = 'pending', retry_count = 0, next_attempt_at = NULL, undo_payload = ?2,
                 seq = (SELECT COALESCE(MAX(seq), 0) + 1 FROM operations)
             WHERE id = ?1 AND status IN ('failed', 'acked')",
            params![id, undo_payload],
        )
        .map_err(sql_err)?;
    }
    Ok(OperationId(id))
}

fn payload_hash(payload: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in payload.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub(crate) fn map_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
    let unread = row.get::<_, i64>(6)? != 0;
    let archived = row.get::<_, i64>(8)? != 0;
    let deleted = row.get::<_, i64>(9)? != 0;
    let snooze: Option<i64> = row.get(13)?;
    let placement = if deleted {
        Placement::Trashed
    } else if archived {
        Placement::Archived { unread }
    } else if let Some(until) = snooze {
        Placement::Snoozed { until, unread }
    } else {
        Placement::Active { unread }
    };
    let importance = match row.get::<_, i64>(11)? {
        ..=-1 => Importance::Low,
        0 => Importance::Normal,
        _ => Importance::High,
    };
    let sender_email = row.get::<_, Option<String>>(15)?.unwrap_or_default();
    let sender_name = row
        .get::<_, Option<String>>(14)?
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| sender_email.clone());
    Ok(Thread {
        id: ThreadId(row.get(0)?),
        account_id: AccountId(row.get(1)?),
        folder: FolderId(row.get(2)?),
        from: Address {
            name: sender_name,
            email: sender_email,
        },
        to: row.get::<_, Option<String>>(16)?.unwrap_or_default(),
        subject: row.get(3)?,
        preview: row.get(4)?,
        date: row.get(5)?,
        placement,
        starred: row.get::<_, i64>(7)? != 0,
        labels: Vec::new(),
        has_attachment: row.get::<_, i64>(10)? != 0,
        importance,
        message_count: row.get::<_, i64>(12)? as u32,
        body_html: String::new(),
        message_id: row.get::<_, Option<String>>(17)?.map(MessageId),
    })
}

/// Kept as a named mapper for callers/tests that distinguish an opened
/// thread from a list row. The shared projection already carries the latest
/// message id, so there is no second query shape to drift from it.
fn map_thread_with_message_id(row: &rusqlite::Row<'_>) -> rusqlite::Result<Thread> {
    map_thread(row)
}

/// The one field-level projection shared by a thread's message list and an
/// account-scoped single-message lookup. Keeping this mapper private to Core
/// prevents the MCP layer from selecting hidden message columns itself.
fn map_thread_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadMessage> {
    Ok(ThreadMessage {
        id: MessageId(row.get(0)?),
        account_id: AccountId(row.get(1)?),
        thread_id: ThreadId(row.get(2)?),
        folder: FolderId(row.get(3)?),
        provider_uid: row.get::<_, Option<i64>>(4)?.map(|value| value as u32),
        message_id_header: row.get(5)?,
        date: row.get(6)?,
        from: Address {
            name: row.get(7)?,
            email: row.get(8)?,
        },
        subject: row.get(9)?,
        unread: row.get::<_, i64>(10)? != 0,
        starred: row.get::<_, i64>(11)? != 0,
        has_attachment: row.get::<_, i64>(12)? != 0,
        size_bytes: row.get::<_, i64>(13)?.max(0) as u64,
    })
}

pub(crate) fn sql_err(e: rusqlite::Error) -> CoreError {
    CoreError::new(ErrorCode::Conflict, "Couldn't save that change.").with_details(e.to_string())
}

pub(crate) fn db_err(e: feathermail_db::Error) -> CoreError {
    CoreError::new(ErrorCode::Conflict, "Couldn't open the mailbox.").with_details(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// T-101: a Reply to a Cyrillic message opens with a readable Subject.
    /// The owner's report was a compose window whose Subject was "a set of
    /// incomprehensible signs" -- the raw `=?UTF-8?B?...?=` word, carried
    /// straight out of `messages.subject`. Mutation: drop the
    /// `decode_encoded_words` call in `response_subject` -> this fails.
    #[test]
    fn a_reply_subject_is_decoded_before_the_re_prefix_goes_on() {
        // "Привет" in UTF-8 base64, and the same word already replied to.
        assert_eq!(
            response_subject("Re:", "=?UTF-8?B?0J/RgNC40LLQtdGC?="),
            "Re: Привет"
        );
        assert_eq!(
            response_subject("Re:", "=?UTF-8?B?UmU6INCf0YDQuNCy0LXRgg==?="),
            "Re: Привет"
        );
        assert_eq!(
            response_subject("Fwd:", "=?UTF-8?B?0J/RgNC40LLQtdGC?="),
            "Fwd: Привет"
        );
        // Plain text is untouched, and an empty subject is still the prefix.
        assert_eq!(
            response_subject("Re:", "Quarterly review"),
            "Re: Quarterly review"
        );
        assert_eq!(response_subject("Re:", "  "), "Re:");
    }

    use crate::mailbox::MailSecurity;
    use crate::model::FIXTURE_NOW;

    fn seed(core: &Core) {
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES ('john', 'John Doe', 'john@example.com', 'generic', 'synced', 'recent', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('inbox', 'john', 'Inbox', 'inbox')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('work', 'john', 'Work', 'custom')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
             VALUES ('t1', 'john', 'inbox', 'Hello', 'Hi there', ?1, 1, 0)",
            params![FIXTURE_NOW],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
             VALUES ('t2', 'john', 'inbox', 'Later', 'Second', ?1, 1, 0)",
            params![FIXTURE_NOW - 60],
        )
        .unwrap();
    }

    fn john() -> AccountId {
        AccountId("john".into())
    }

    fn draft_content(body: &str) -> DraftContent {
        DraftContent {
            from: "john@example.com".into(),
            to: "jane@example.com".into(),
            subject: "Hello".into(),
            body: body.into(),
            ..DraftContent::default()
        }
    }

    /// T-108: a second mailbox, so the unified view has something to merge.
    fn seed_second_account(core: &Core) {
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES ('jane', 'Jane Roe', 'jane@example.com', 'generic', 'synced', 'recent', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('jane:inbox', 'jane', 'Inbox', 'inbox')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('jane:sent', 'jane', 'Sent', 'sent')",
            [],
        )
        .unwrap();
        // Newer than `seed`'s t1, so a merged page has to interleave rather
        // than simply concatenate the two mailboxes.
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
             VALUES ('j1', 'jane', 'jane:inbox', 'From Jane', 'Newest', ?1, 1, 0)",
            params![FIXTURE_NOW + 60],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
             VALUES ('j2', 'jane', 'jane:sent', 'Sent by Jane', 'Reply', ?1, 0, 0)",
            params![FIXTURE_NOW - 30],
        )
        .unwrap();
    }

    /// T-108: the owner asked for one view over every mailbox. Both accounts'
    /// Inboxes come back in one page, newest first, and each thread still
    /// says which account it belongs to -- that is what the shell aims an
    /// action at, since a merged page has no single account of its own.
    #[test]
    fn the_unified_inbox_merges_every_account_newest_first() {
        let core = Core::memory().unwrap();
        seed(&core);
        seed_second_account(&core);
        let page = core
            .list_unified_threads(UnifiedThreadsQuery {
                kind: FolderKind::Inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: LIST_PAGE,
            })
            .unwrap();
        let ids: Vec<&str> = page.threads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["j1", "t1", "t2"], "one order over both mailboxes");
        assert_eq!(page.total, 3);
        let accounts: Vec<&str> = page.threads.iter().map(|t| t.account_id.as_str()).collect();
        assert_eq!(accounts, ["jane", "john", "john"]);

        // T-115: the merged view can lose the row (a reload, the first-paint
        // tail) and still has to know which mailbox to fetch the body on.
        // Guessing from the account menu is the wrong session.
        assert_eq!(
            core.account_id_for_thread(&ThreadId("j1".into()))
                .unwrap()
                .as_str(),
            "jane"
        );
        assert_eq!(
            core.account_id_for_thread(&ThreadId("t1".into()))
                .unwrap()
                .as_str(),
            "john"
        );
        assert_eq!(
            core.account_id_for_thread(&ThreadId("missing".into()))
                .unwrap_err()
                .code,
            ErrorCode::MessageNotFound
        );

        // Sent is the other real folder kind in the merged view: it must
        // pick up each account's own Sent row and nothing else.
        let sent = core
            .list_unified_threads(UnifiedThreadsQuery {
                kind: FolderKind::Sent,
                filter: ThreadFilter::All,
                after: None,
                limit: LIST_PAGE,
            })
            .unwrap();
        let sent_ids: Vec<&str> = sent.threads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(sent_ids, ["j2"]);
    }

    /// T-108: the merged view is four folders, and the four are the ones the
    /// owner named. Anything else is an error rather than an empty list --
    /// an empty Drafts would look like a mailbox with no drafts, which is a
    /// different (and false) statement.
    #[test]
    fn the_unified_view_offers_exactly_inbox_sent_starred_and_trash() {
        let core = Core::memory().unwrap();
        seed(&core);
        seed_second_account(&core);
        let folders = core.list_unified_folders().unwrap();
        let kinds: Vec<FolderKind> = folders.iter().map(|f| f.folder.kind).collect();
        assert_eq!(
            kinds,
            vec![
                FolderKind::Inbox,
                FolderKind::Sent,
                FolderKind::Starred,
                FolderKind::Trash
            ]
        );
        let inbox = &folders[0];
        assert_eq!(inbox.folder.id.as_str(), "unified:inbox");
        assert_eq!(inbox.total, 3, "both mailboxes count toward one row");
        assert_eq!(inbox.unread, 3);

        for kind in [FolderKind::Drafts, FolderKind::Spam, FolderKind::Archive] {
            let refused = core.list_unified_threads(UnifiedThreadsQuery {
                kind,
                filter: ThreadFilter::All,
                after: None,
                limit: LIST_PAGE,
            });
            assert!(refused.is_err(), "{kind:?} is not part of the merged view");
        }
    }

    /// T-108 + T-106: "mark all as read" on the merged Inbox has to reach
    /// every account's Inbox, not just the first one.
    #[test]
    fn marking_the_unified_inbox_read_reaches_every_account() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        seed_second_account(&core);
        let receipt = core.mark_unified_folder_read(FolderKind::Inbox).unwrap();
        assert_eq!(receipt.operations.len(), 3);
        let unread: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM threads WHERE unread = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(unread, 0);
    }

    /// T-106: the owner's "mark all as read" on a sidebar folder. Both
    /// unread threads in the Inbox go read in one call, each with its own
    /// queued operation so the flag reaches the server, and the receipt
    /// says how many -- that count is what the toast shows.
    #[test]
    fn marking_a_folder_read_clears_every_unread_thread_in_it() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        let receipt = core
            .mark_folder_read(&john(), &FolderId("inbox".into()))
            .unwrap();
        assert_eq!(receipt.operations.len(), 2);
        let unread: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE account_id = 'john' AND unread = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unread, 0, "no thread in the folder may stay unread");
        let queued: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE op = 'mark_read'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(queued, 2, "each thread needs its own \\Seen store");
    }

    /// T-106: the menu item is insensitive on a folder with nothing unread,
    /// but the door behind it must still be safe to open -- an empty folder
    /// is a no-op with an empty receipt, not `InvalidArgument` (which is
    /// what dispatching `MarkRead` with no ids would have returned).
    #[test]
    fn marking_a_folder_with_nothing_unread_is_a_quiet_no_op() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.mark_folder_read(&john(), &FolderId("inbox".into()))
            .unwrap();
        let again = core
            .mark_folder_read(&john(), &FolderId("inbox".into()))
            .unwrap();
        assert!(again.operations.is_empty());
    }

    /// T-106: "all" means the folder the row names, not every folder. The
    /// custom folder's unread thread survives the Inbox's mark-all.
    #[test]
    fn marking_one_folder_read_leaves_the_other_folders_alone() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.db
            .conn()
            .execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
                 VALUES ('t3', 'john', 'work', 'Work', 'Third', ?1, 1, 0)",
                params![FIXTURE_NOW - 120],
            )
            .unwrap();
        core.mark_folder_read(&john(), &FolderId("inbox".into()))
            .unwrap();
        let still_unread: i64 = core
            .db
            .conn()
            .query_row("SELECT unread FROM threads WHERE id = 't3'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(still_unread, 1);
    }

    #[test]
    fn draft_autosave_updates_one_row_instead_of_creating_a_copy() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(10);
        let first = core
            .save_draft(&john(), None, draft_content("one"))
            .unwrap();
        core.set_now(11);
        let second = core
            .save_draft(&john(), Some(&first.id), draft_content("two"))
            .unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.updated_at, 11);
        assert_eq!(second.body, "two");
        assert_eq!(core.list_drafts(&john()).unwrap().len(), 1);
    }

    #[test]
    fn response_drafts_preserve_reply_threading_and_reply_all_recipients() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(10);
        core.db
            .conn()
            .execute(
                "INSERT INTO messages
                 (id, account_id, thread_id, folder_id, message_id_header,
                  references_header, date, sender_name, sender_email, recipients, cc, subject)
                 VALUES ('m1', 'john', 't1', 'inbox', '<parent@example.test>',
                  '<root@example.test>', 10, 'Author', 'author@example.test',
                  'john@example.com, teammate@example.test',
                  'copy@example.test, author@example.test', 'Quarterly update')",
                [],
            )
            .unwrap();

        let reply = core
            .create_response_draft(
                &john(),
                &MessageId("m1".into()),
                ResponseKind::ReplyAll,
                "\n\n> quoted source".into(),
            )
            .unwrap();
        assert_eq!(reply.thread_id.as_ref().map(ThreadId::as_str), Some("t1"));
        assert_eq!(
            reply.in_reply_to.as_ref().map(MessageId::as_str),
            Some("m1")
        );
        assert_eq!(reply.from, "john@example.com");
        assert_eq!(reply.to, "author@example.test");
        assert_eq!(reply.cc, "teammate@example.test, copy@example.test");
        assert_eq!(reply.subject, "Re: Quarterly update");
        assert_eq!(reply.body, "\n\n> quoted source");

        let operation = core.queue_draft_send(&john(), &reply.id).unwrap();
        let outbox_id: String = core
            .db
            .conn()
            .query_row(
                "SELECT target_id FROM operations WHERE id = ?1",
                params![operation.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        let outbox = core.load_outbox(&john(), &outbox_id).unwrap();
        assert_eq!(outbox.in_reply_to.as_deref(), Some("<parent@example.test>"));
        assert_eq!(
            outbox.references.as_deref(),
            Some("<root@example.test> <parent@example.test>")
        );

        core.db
            .conn()
            .execute("UPDATE messages SET sender_email = '' WHERE id = 'm1'", [])
            .unwrap();
        let forward = core
            .create_response_draft(
                &john(),
                &MessageId("m1".into()),
                ResponseKind::Forward,
                "\n\n> quoted source".into(),
            )
            .unwrap();
        assert_eq!(forward.thread_id, None);
        assert_eq!(forward.in_reply_to, None);
        assert_eq!(forward.to, "");
        assert_eq!(forward.cc, "");
        assert_eq!(forward.subject, "Fwd: Quarterly update");
    }

    #[test]
    fn draft_attachment_keeps_only_disk_metadata_and_can_be_removed() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(10);
        let draft = core
            .save_draft(&john(), None, draft_content("private compose text"))
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quarterly-report.pdf");
        std::fs::write(&path, b"private attachment bytes").unwrap();

        let attached = core.attach_to_draft(&john(), &draft.id, &path).unwrap();
        assert_eq!(attached.filename, "quarterly-report.pdf");
        assert_eq!(attached.mime, "application/pdf");
        assert_eq!(attached.size_bytes, 24);
        assert_eq!(attached.source_path, path);

        let listed = core.list_draft_attachments(&john(), &draft.id).unwrap();
        assert_eq!(listed, vec![attached.clone()]);
        let stored: (String, i64, String) = core
            .db
            .conn()
            .query_row(
                "SELECT filename, size_bytes, source_path FROM draft_attachments WHERE id = ?1",
                params![attached.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored.0, "quarterly-report.pdf");
        assert_eq!(stored.1, 24);
        assert_eq!(stored.2, path.to_string_lossy());
        assert!(!stored.2.contains("private attachment bytes"));

        assert!(core
            .remove_draft_attachment(&john(), &draft.id, &attached.id)
            .unwrap());
        assert!(core
            .list_draft_attachments(&john(), &draft.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn draft_attachment_over_the_limit_is_rejected_without_a_row() {
        let core = Core::memory().unwrap();
        seed(&core);
        let draft = core
            .save_draft(&john(), None, draft_content("private compose text"))
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("too-large.bin");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_OUTGOING_ATTACHMENT_BYTES + 1)
            .unwrap();

        let err = core.attach_to_draft(&john(), &draft.id, &path).unwrap_err();
        assert_eq!(err.code, ErrorCode::AttachmentTooLarge);
        assert_eq!(err.message, "Attachments must be 100 MB or smaller.");
        assert!(core
            .list_draft_attachments(&john(), &draft.id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn drafts_remote_folder_rejects_an_empty_server_mailbox_name() {
        let core = Core::memory().unwrap();
        seed(&core);
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, remote_id, name, kind) \
                 VALUES ('drafts', 'john', '', 'Drafts', 'drafts')",
                [],
            )
            .unwrap();

        assert_eq!(core.drafts_remote_folder(&john()).unwrap(), None);
    }

    #[test]
    fn newer_draft_revision_cancels_the_old_upload_without_putting_body_in_queue() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(10);
        let first = core
            .save_draft(&john(), None, draft_content("first private revision"))
            .unwrap();
        core.set_now(10);
        let second = core
            .save_draft(
                &john(),
                Some(&first.id),
                draft_content("second private revision"),
            )
            .unwrap();
        assert_eq!(first.id, second.id);

        let rows: Vec<(String, String)> = core
            .db
            .conn()
            .prepare(
                "SELECT payload, status FROM operations
                 WHERE account_id = 'john' AND target_id = ?1 AND op = 'sync_draft'
                 ORDER BY payload",
            )
            .unwrap()
            .query_map(params![first.id.as_str()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("1".into(), "cancelled".into()),
                ("2".into(), "pending".into())
            ]
        );
        assert!(
            rows.iter()
                .all(|(payload, _)| !payload.contains("private revision")),
            "draft text must remain in drafts, not an operations payload"
        );
        assert!(core
            .draft_for_sync(&john(), &first.id, 1)
            .unwrap()
            .is_none());
        assert_eq!(
            core.draft_for_sync(&john(), &first.id, 2)
                .unwrap()
                .unwrap()
                .body,
            "second private revision"
        );
    }

    struct RejectDraftUpload;

    impl crate::provider::MailProvider for RejectDraftUpload {
        fn apply(
            &mut self,
            _op: &crate::model::Operation,
        ) -> Result<(), crate::provider::ApplyError> {
            Err(crate::provider::ApplyError::Unsupported)
        }
    }

    #[test]
    fn failed_draft_upload_never_removes_the_local_draft() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        let draft = core
            .save_draft(&john(), None, draft_content("keep this local"))
            .unwrap();
        let outcome = core.tick(&mut RejectDraftUpload).unwrap();
        assert!(matches!(
            outcome,
            crate::queue::TickOutcome::Failed {
                error: crate::provider::ApplyError::Unsupported,
                ..
            }
        ));
        assert_eq!(
            core.get_draft(&john(), &draft.id).unwrap().body,
            "keep this local"
        );
    }

    #[test]
    fn queue_send_refuses_a_to_field_that_is_not_an_address() {
        let core = Core::memory().unwrap();
        seed(&core);
        let mut content = draft_content("hi");
        content.to = "не-адрес".into();
        let draft = core.save_draft(&john(), None, content).unwrap();
        let err = core.queue_draft_send(&john(), &draft.id).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.message, "That doesn’t look like an address.");
        let queued: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, 0);
    }

    #[test]
    fn queue_send_freezes_the_draft_body_outside_the_operation_payload() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(10);
        let draft = core
            .save_draft(&john(), None, draft_content("original"))
            .unwrap();
        let op = core.queue_draft_send(&john(), &draft.id).unwrap();
        core.save_draft(&john(), Some(&draft.id), draft_content("edited later"))
            .unwrap();
        let outbox_id: String = core
            .db
            .conn()
            .query_row(
                "SELECT target_id FROM operations WHERE id = ?1",
                params![op.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        let queued = core.load_outbox(&john(), &outbox_id).unwrap();
        assert_eq!(queued.body, "original");
        let payload: String = core
            .db
            .conn()
            .query_row(
                "SELECT payload FROM operations WHERE id = ?1",
                params![op.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(payload, "{}");
        assert!(!format!("{queued:?}").contains("original"));
    }

    struct OfflineSmtp;

    impl crate::provider::MailProvider for OfflineSmtp {
        fn apply(
            &mut self,
            _op: &crate::model::Operation,
        ) -> Result<(), crate::provider::ApplyError> {
            Err(crate::provider::ApplyError::Network)
        }
    }

    #[test]
    fn smtp_network_failure_keeps_the_frozen_outbox_message_retryable() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(10);
        let draft = core
            .save_draft(&john(), None, draft_content("keep this message"))
            .unwrap();
        // This test owns the Send operation. T-042's independently queued
        // draft sync would otherwise be the oldest operation for the same
        // account and obscure the SMTP retry branch being asserted here.
        core.db
            .conn()
            .execute(
                "UPDATE operations SET status = 'cancelled' \
                 WHERE account_id = 'john' AND op = 'sync_draft'",
                [],
            )
            .unwrap();
        let operation = core.queue_draft_send(&john(), &draft.id).unwrap();
        let outbox_id: String = core
            .db
            .conn()
            .query_row(
                "SELECT target_id FROM operations WHERE id = ?1",
                params![operation.as_str()],
                |row| row.get(0),
            )
            .unwrap();

        let outcome = core.tick(&mut OfflineSmtp).unwrap();
        assert!(matches!(
            outcome,
            crate::queue::TickOutcome::Retry { id, delay: 2 } if id == operation
        ));
        assert_eq!(
            core.load_outbox(&john(), &outbox_id).unwrap().status,
            "queued",
            "SMTP never accepted the message, so its durable snapshot must remain in Outbox"
        );
        let (status, retry_count, next_attempt): (String, i64, Option<i64>) = core
            .db
            .conn()
            .query_row(
                "SELECT status, retry_count, next_attempt_at FROM operations WHERE id = ?1",
                params![operation.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(retry_count, 1);
        assert_eq!(next_attempt, Some(12));
    }

    fn tid(id: &str) -> ThreadId {
        ThreadId(id.into())
    }

    #[test]
    fn list_and_get_thread_project_latest_message_sender_and_id() {
        let core = Core::memory().unwrap();
        seed(&core);
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO messages
             (id, account_id, thread_id, folder_id, date, sender_name, sender_email, recipients)
             VALUES ('old', 'john', 't1', 'inbox', ?1, 'Older', 'older@example.com', 'john@example.com')",
            params![FIXTURE_NOW - 60],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages
             (id, account_id, thread_id, folder_id, date, sender_name, sender_email, recipients)
             VALUES ('new', 'john', 't1', 'inbox', ?1, 'Newest', 'newest@example.com', 'john@example.com')",
            params![FIXTURE_NOW],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages
             (id, account_id, thread_id, folder_id, date, sender_name, sender_email, recipients)
             VALUES ('no-name', 'john', 't2', 'inbox', ?1, '', 'receipt@example.com', 'john@example.com')",
            params![FIXTURE_NOW - 60],
        )
        .unwrap();

        let query = ListThreadsQuery {
            account_id: john(),
            folder_id: FolderId("inbox".into()),
            filter: ThreadFilter::All,
            after: None,
            limit: 64,
        };
        let listed = core.list_threads(query).unwrap();
        assert_eq!(listed.threads[0].from.name, "Newest");
        assert_eq!(listed.threads[0].from.email, "newest@example.com");
        assert_eq!(listed.threads[0].to, "john@example.com");
        assert_eq!(listed.threads[0].message_id, Some(MessageId("new".into())));
        assert_eq!(listed.threads[1].from.name, "receipt@example.com");

        let opened = core.get_thread(&john(), &tid("t1")).unwrap();
        assert_eq!(opened.from.name, "Newest");
        assert_eq!(opened.from.email, "newest@example.com");
        assert_eq!(opened.to, "john@example.com");
        assert_eq!(opened.message_id, Some(MessageId("new".into())));
    }

    #[test]
    fn get_thread_message_is_account_scoped_metadata_only() {
        let core = Core::memory().unwrap();
        seed(&core);
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO messages
             (id, account_id, thread_id, folder_id, provider_uid, message_id_header,
              date, sender_name, sender_email, subject, unread, starred,
              has_attachment, size_bytes)
             VALUES ('message-john', 'john', 't1', 'inbox', 7, '<john@example.test>',
                     ?1, 'John Sender', 'john@example.test', 'Safe metadata', 1, 1, 1, 42)",
            params![FIXTURE_NOW],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES ('other', 'Other', 'other@example.test', 'generic', 'synced', 'recent', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind)
             VALUES ('other-inbox', 'other', 'Inbox', 'inbox')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
             VALUES ('other-thread', 'other', 'other-inbox', 'Other', '', ?1, 0, 0)",
            params![FIXTURE_NOW],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages
             (id, account_id, thread_id, folder_id, date, sender_name, sender_email)
             VALUES ('message-other', 'other', 'other-thread', 'other-inbox',
                     ?1, 'Other Sender', 'other@example.test')",
            params![FIXTURE_NOW],
        )
        .unwrap();

        let message = core
            .get_thread_message(&john(), &MessageId("message-john".into()))
            .unwrap();
        assert_eq!(message.id.as_str(), "message-john");
        assert_eq!(message.account_id, john());
        assert_eq!(message.thread_id.as_str(), "t1");
        assert_eq!(message.folder.as_str(), "inbox");
        assert_eq!(message.from.email, "john@example.test");
        assert_eq!(message.subject, "Safe metadata");
        assert!(message.unread && message.starred && message.has_attachment);

        for message_id in ["missing-message", "message-other"] {
            assert_eq!(
                core.get_thread_message(&john(), &MessageId(message_id.into()))
                    .unwrap_err()
                    .code,
                ErrorCode::MessageNotFound,
                "unknown and foreign messages have the same fail-closed outcome"
            );
        }
    }

    #[test]
    fn archive_writes_db_and_notifies_without_gtk() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let rx = core.subscribe();
        core.dispatch(Command::Archive {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        })
        .unwrap();
        let t = core.get_thread(&john(), &tid("t1")).unwrap();
        assert!(t.archived());
        assert!(!t.unread());
        let inbox = core
            .list_threads(ListThreadsQuery {
                account_id: john(),
                folder_id: FolderId("inbox".into()),
                filter: ThreadFilter::All,
                after: None,
                limit: 64,
            })
            .unwrap();
        assert_eq!(inbox.total, 1);
        assert_eq!(inbox.threads[0].id.as_str(), "t2");
        let archived = core
            .list_threads(ListThreadsQuery {
                account_id: john(),
                folder_id: FolderId("archive".into()),
                filter: ThreadFilter::All,
                after: None,
                limit: 64,
            })
            .unwrap();
        assert_eq!(archived.total, 1);
        let (op, status): (String, String) = core
            .db
            .conn()
            .query_row(
                "SELECT op, status FROM operations WHERE target_id = 't1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(op, "archive");
        assert_eq!(status, "pending");
        let event = rx.try_recv().unwrap();
        assert_eq!(
            event,
            MailEvent::ThreadsChanged {
                account_id: john(),
                thread_ids: vec![tid("t1")],
            }
        );
    }

    /// T-036/T-060: a 1,000-row selection is one Core command transaction,
    /// carrying one vector through `dispatch_with_receipt`; it is not 1,000
    /// UI dispatches. The receipt/event and resulting rows make the batch
    /// boundary observable without coupling the test to SQLite internals.
    #[test]
    fn archive_batch_of_one_thousand_threads_uses_one_dispatch_transaction() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        {
            let conn = core.db.conn();
            for index in 0..1_000 {
                conn.execute(
                    "INSERT INTO threads \
                     (id, account_id, folder_id, subject, snippet, date, unread, starred) \
                     VALUES (?1, 'john', 'inbox', ?2, 'Batch', ?3, 1, 0)",
                    params![
                        format!("bulk-{index:04}"),
                        format!("Bulk {index}"),
                        FIXTURE_NOW - index as i64,
                    ],
                )
                .unwrap();
            }
        }
        let ids: Vec<ThreadId> = (0..1_000)
            .map(|index| ThreadId(format!("bulk-{index:04}")))
            .collect();
        let event_rx = core.subscribe();
        let receipt = core
            .dispatch_with_receipt(Command::Archive {
                account_id: john(),
                thread_ids: ids.clone(),
            })
            .unwrap();

        assert_eq!(receipt.operations.len(), 1_000);
        let event = event_rx.try_recv().unwrap();
        assert_eq!(
            event,
            MailEvent::ThreadsChanged {
                account_id: john(),
                thread_ids: ids,
            }
        );
        let (archived, operations): (i64, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM threads WHERE account_id = 'john' AND archived = 1),
                    (SELECT COUNT(*) FROM operations WHERE account_id = 'john' AND op = 'archive')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(archived, 1_000);
        assert_eq!(operations, 1_000);
    }

    #[test]
    fn snooze_persists_deadline_and_wakes_to_inbox_at_frozen_time() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let until = FIXTURE_NOW + 3_600;
        let receipt = core
            .dispatch_with_receipt(Command::Snooze {
                account_id: john(),
                thread_ids: vec![tid("t1")],
                until,
            })
            .unwrap();
        assert_eq!(receipt.operations.len(), 1);
        let stored: (i64, String) = core
            .db
            .conn()
            .query_row(
                "SELECT until_ts, previous_folder_id
                 FROM snoozes
                 WHERE snoozes.thread_id = 't1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, (until, "inbox".into()));
        let (status, operation_count): (String, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT status, COUNT(*) FROM operations WHERE op = 'snooze'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "local");
        assert_eq!(operation_count, 1);
        assert_eq!(
            core.next_snooze_deadline(Some(&john())).unwrap(),
            Some(until)
        );

        core.set_now(until - 1);
        assert!(core.wake_due_snoozes().unwrap().is_empty());
        assert_eq!(
            core.get_thread(&john(), &tid("t1"))
                .unwrap()
                .snoozed_until(),
            Some(until)
        );

        core.set_now(until);
        let woken = core.wake_due_snoozes().unwrap();
        assert_eq!(woken, vec![(john(), tid("t1"))]);
        let thread = core.get_thread(&john(), &tid("t1")).unwrap();
        assert_eq!(thread.folder.as_str(), "inbox");
        assert_eq!(thread.snoozed_until(), None);
        assert_eq!(core.next_snooze_deadline(None).unwrap(), None);
        let n: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM snoozes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn unsnooze_thread_runs_the_same_transition_the_timer_would_have() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let until = FIXTURE_NOW + 3_600;
        core.dispatch_with_receipt(Command::Snooze {
            account_id: john(),
            thread_ids: vec![tid("t1")],
            until,
        })
        .unwrap();
        assert_eq!(
            core.get_thread(&john(), &tid("t1"))
                .unwrap()
                .snoozed_until(),
            Some(until)
        );

        // Well before the deadline: the timer would do nothing here.
        core.set_now(until - 1_800);
        assert!(core.wake_due_snoozes().unwrap().is_empty());
        assert!(core.unsnooze_thread(&john(), &tid("t1")).unwrap());

        let thread = core.get_thread(&john(), &tid("t1")).unwrap();
        assert_eq!(thread.folder.as_str(), "inbox");
        assert_eq!(thread.snoozed_until(), None);
        assert!(!thread.archived());
        assert_eq!(core.next_snooze_deadline(None).unwrap(), None);
        let rows: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM snoozes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0, "the snooze itself must be gone");
        let status: String = core
            .db
            .conn()
            .query_row(
                "SELECT status FROM operations WHERE op = 'snooze'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "cancelled",
            "the local snooze ledger row is retired"
        );

        // And the timer has nothing left to wake once the deadline passes.
        core.set_now(until + 1);
        assert!(core.wake_due_snoozes().unwrap().is_empty());
    }

    #[test]
    fn unsnooze_thread_reports_false_when_the_thread_is_not_snoozed() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        assert!(!core.unsnooze_thread(&john(), &tid("t1")).unwrap());
        // An unknown thread in a known account is simply "not snoozed", not an
        // error: the caller asked for an end state that already holds.
        assert!(!core.unsnooze_thread(&john(), &tid("nope")).unwrap());
        let thread = core.get_thread(&john(), &tid("t1")).unwrap();
        assert_eq!(thread.snoozed_until(), None);
    }

    #[test]
    fn unsnooze_thread_rejects_an_unknown_account() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let err = core
            .unsnooze_thread(&AccountId("ghost".into()), &tid("t1"))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);
    }

    #[test]
    fn unsnooze_thread_touches_only_the_named_thread() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let until = FIXTURE_NOW + 3_600;
        core.dispatch_with_receipt(Command::Snooze {
            account_id: john(),
            thread_ids: vec![tid("t1"), tid("t2")],
            until,
        })
        .unwrap();
        assert!(core.unsnooze_thread(&john(), &tid("t1")).unwrap());
        assert_eq!(
            core.get_thread(&john(), &tid("t2"))
                .unwrap()
                .snoozed_until(),
            Some(until),
            "the sibling snooze must survive"
        );
        assert_eq!(
            core.next_snooze_deadline(Some(&john())).unwrap(),
            Some(until)
        );
    }

    #[test]
    fn snooze_undo_restores_previous_local_state_without_provider_operation() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let receipt = core
            .dispatch_with_receipt(Command::Snooze {
                account_id: john(),
                thread_ids: vec![tid("t1")],
                until: FIXTURE_NOW + 3_600,
            })
            .unwrap();
        let ticket = receipt.operations[0].undo_ticket();
        let undone = core.undo(&ticket).unwrap();
        assert!(matches!(undone, UndoReceipt::Cancelled { .. }));
        assert_eq!(
            core.get_thread(&john(), &tid("t1"))
                .unwrap()
                .snoozed_until(),
            None
        );
        assert_eq!(core.next_snooze_deadline(Some(&john())).unwrap(), None);
        assert_eq!(
            core.db
                .conn()
                .query_row(
                    "SELECT status FROM operations WHERE id = ?1",
                    params![ticket.operation_id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "cancelled"
        );
        assert_eq!(
            core.db
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM operations WHERE status = 'pending'",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn snooze_undo_restores_previous_timestamp() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        core.dispatch(Command::Snooze {
            account_id: john(),
            thread_ids: vec![tid("t1")],
            until: FIXTURE_NOW + 1_000,
        })
        .unwrap();
        let second_until = FIXTURE_NOW + 2_000;
        let receipt = core
            .dispatch_with_receipt(Command::Snooze {
                account_id: john(),
                thread_ids: vec![tid("t1")],
                until: second_until,
            })
            .unwrap();
        let ticket = receipt.operations[0].undo_ticket();
        core.undo(&ticket).unwrap();
        assert_eq!(
            core.get_thread(&john(), &tid("t1"))
                .unwrap()
                .snoozed_until(),
            Some(FIXTURE_NOW + 1_000)
        );
        assert_eq!(
            core.next_snooze_deadline(Some(&john())).unwrap(),
            Some(FIXTURE_NOW + 1_000)
        );
    }

    #[test]
    fn snooze_undo_ticket_survives_restart_without_entering_provider_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let ticket = {
            let mut core = Core::open(&path).unwrap();
            core.set_now(FIXTURE_NOW);
            seed(&core);
            core.dispatch_with_receipt(Command::Snooze {
                account_id: john(),
                thread_ids: vec![tid("t1")],
                until: FIXTURE_NOW + 3_600,
            })
            .unwrap()
            .operations[0]
                .undo_ticket()
        };

        let mut reopened = Core::open(&path).unwrap();
        reopened.set_now(FIXTURE_NOW + 10);
        reopened.undo(&ticket).unwrap();
        assert_eq!(
            reopened
                .get_thread(&john(), &tid("t1"))
                .unwrap()
                .snoozed_until(),
            None
        );
        assert_eq!(reopened.next_snooze_deadline(Some(&john())).unwrap(), None);
        assert_eq!(
            reopened
                .db
                .conn()
                .query_row(
                    "SELECT status FROM operations WHERE id = ?1",
                    params![ticket.operation_id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "cancelled"
        );
    }

    #[test]
    fn snooze_due_before_reopen_is_woken_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let mut core = Core::open(&path).unwrap();
            core.set_now(FIXTURE_NOW);
            seed(&core);
            core.dispatch(Command::Snooze {
                account_id: john(),
                thread_ids: vec![tid("t1")],
                until: FIXTURE_NOW + 10,
            })
            .unwrap();
        }
        let mut reopened = Core::open(&path).unwrap();
        reopened.set_now(FIXTURE_NOW + 10);
        assert_eq!(reopened.wake_due_snoozes().unwrap().len(), 1);
        assert_eq!(
            reopened
                .get_thread(&john(), &tid("t1"))
                .unwrap()
                .snoozed_until(),
            None
        );
    }

    #[test]
    fn trash_cannot_be_marked_unread() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.dispatch(Command::Trash {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        })
        .unwrap();
        core.dispatch(Command::MarkUnread {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        })
        .unwrap();
        let t = core.get_thread(&john(), &tid("t1")).unwrap();
        assert!(t.deleted());
        assert!(!t.unread());
    }

    #[test]
    fn permanent_delete_is_a_distinct_queued_command_and_reaches_a_trashed_thread() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);

        // A thread already in the local Trash overlay still needs a remote
        // permanent-delete operation; this is not the same as repeating
        // the idempotent Trash command.
        core.dispatch(Command::Trash {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        })
        .unwrap();
        core.dispatch(Command::PermanentDelete {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        })
        .unwrap();

        let t = core.get_thread(&john(), &tid("t1")).unwrap();
        assert!(t.deleted());
        let kinds: Vec<String> = core
            .db
            .conn()
            .prepare("SELECT op FROM operations WHERE target_id = 't1' ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(kinds.len(), 2);
        assert!(kinds.contains(&"trash".to_string()));
        assert!(kinds.contains(&"permanent_delete".to_string()));
    }

    #[test]
    fn unknown_account_is_data() {
        let mut core = Core::memory().unwrap();
        let err = core
            .dispatch(Command::Archive {
                account_id: AccountId("nope".into()),
                thread_ids: vec![tid("t1")],
            })
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);
    }

    #[test]
    fn list_threads_cursor_pages() {
        let core = Core::memory().unwrap();
        seed(&core);
        let first = core
            .list_threads(ListThreadsQuery {
                account_id: john(),
                folder_id: FolderId("inbox".into()),
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap();
        assert_eq!(first.threads.len(), 1);
        assert_eq!(first.threads[0].id.as_str(), "t1");
        assert_eq!(first.total, 2);
        assert!(first.next.is_some());
        let second = core
            .list_threads(ListThreadsQuery {
                account_id: john(),
                folder_id: FolderId("inbox".into()),
                filter: ThreadFilter::All,
                after: first.next,
                limit: 1,
            })
            .unwrap();
        assert_eq!(second.threads[0].id.as_str(), "t2");
        assert!(second.next.is_none());
    }

    /// D15 has to constrain the SQL `Core::list_threads` actually builds,
    /// not just a parallel db-only acceptance query. The helper under test is
    /// the production source fragment for concrete Inbox/Custom folders.
    #[test]
    fn concrete_folder_thread_page_keeps_the_d15_folder_index() {
        let core = Core::memory().unwrap();
        seed(&core);
        let source = thread_page_source(true);
        let sql = format!(
            "SELECT t.id FROM {source} \
             WHERE t.account_id = ?1 AND t.folder_id = ?2 \
               AND t.archived = 0 AND t.deleted = 0 AND t.snooze_until IS NULL \
             ORDER BY t.date DESC, t.id DESC LIMIT 64"
        );
        let plan = core
            .db
            .explain_query_plan(&sql, &[&"john", &"inbox"])
            .unwrap();
        assert!(
            plan.contains("threads_account_folder_date"),
            "concrete folder page must use the D15 index, plan was:\n{plan}"
        );
    }

    /// T-037: Filter is a Core query constraint, not a post-filter on the
    /// first GTK page. A matching older row must therefore still arrive in
    /// a one-row page, and a matching row in another account or folder must
    /// never leak into this account/folder combination.
    #[test]
    fn list_threads_filter_is_exact_across_page_folder_and_account() {
        let core = Core::memory().unwrap();
        seed(&core);
        let conn = core.db.conn();
        conn.execute(
            "UPDATE threads SET unread = 1, starred = 0, has_attachment = 0 WHERE id = 't1'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE threads SET unread = 0, starred = 1, has_attachment = 1 WHERE id = 't2'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads \
             (id, account_id, folder_id, subject, snippet, date, unread, starred, has_attachment) \
             VALUES ('work-star', 'john', 'work', 'Work only', '', ?1, 0, 1, 1)",
            params![FIXTURE_NOW + 120],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO accounts \
             (id, name, email, provider, status, download_policy, created_at, updated_at) \
             VALUES ('jane', 'Jane Roe', 'jane@example.com', 'generic', 'synced', 'recent', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads \
             (id, account_id, folder_id, subject, snippet, date, unread, starred, has_attachment) \
             VALUES ('jane-star', 'jane', 'inbox', 'Another account', '', ?1, 1, 1, 1)",
            params![FIXTURE_NOW + 60],
        )
        .unwrap();

        let query = |filter| ListThreadsQuery {
            account_id: john(),
            folder_id: FolderId("inbox".into()),
            filter,
            after: None,
            limit: 1,
        };

        let all = core.list_threads(query(ThreadFilter::All)).unwrap();
        assert_eq!(all.total, 2);
        assert_eq!(all.threads[0].id.as_str(), "t1");
        assert!(all.next.is_some());

        let unread = core.list_threads(query(ThreadFilter::Unread)).unwrap();
        assert_eq!(unread.total, 1);
        assert_eq!(unread.threads[0].id.as_str(), "t1");
        assert!(unread.next.is_none());

        let starred = core.list_threads(query(ThreadFilter::Starred)).unwrap();
        assert_eq!(starred.total, 1);
        assert_eq!(starred.threads[0].id.as_str(), "t2");
        assert!(starred.next.is_none());

        let attachments = core.list_threads(query(ThreadFilter::Attachments)).unwrap();
        assert_eq!(attachments.total, 1);
        assert_eq!(attachments.threads[0].id.as_str(), "t2");
        assert!(attachments.next.is_none());
    }

    #[test]
    fn archive_is_idempotent_in_queue() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let cmd = Command::Archive {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        };
        core.dispatch(cmd.clone()).unwrap();
        core.dispatch(cmd).unwrap();
        let n: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn dispatch_receipt_maps_each_archive_thread_to_its_queue_id() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let cmd = Command::Archive {
            account_id: john(),
            thread_ids: vec![tid("t1"), tid("t2")],
        };

        let first = core.dispatch_with_receipt(cmd.clone()).unwrap();
        assert_eq!(first.operations.len(), 2);
        for receipt in &first.operations {
            let stored: String = core
                .db
                .conn()
                .query_row(
                    "SELECT id FROM operations WHERE target_id = ?1",
                    params![receipt.thread_id.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(receipt.operation_id.as_str(), stored);
        }

        // Re-expose the same target while the original operation is still
        // pending. The second dispatch must return the same deterministic
        // id, not create a second queue row or make the caller hash it.
        core.db
            .conn()
            .execute("UPDATE threads SET archived = 0", [])
            .unwrap();
        let second = core.dispatch_with_receipt(cmd).unwrap();
        assert_eq!(
            first.operations, second.operations,
            "deduplicated dispatch must preserve the exact queue ids"
        );
        let count: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    struct OkConnector;
    struct AuthFailConnector;

    impl MailConnector for OkConnector {
        fn probe(
            &self,
            _: &MailboxForm,
            _: &str,
        ) -> Result<crate::provider::ConnectOk, crate::provider::ConnectError> {
            Ok(crate::provider::ConnectOk {
                capabilities: vec!["IMAP4rev1".into()],
            })
        }
    }

    impl MailConnector for AuthFailConnector {
        fn probe(
            &self,
            _: &MailboxForm,
            _: &str,
        ) -> Result<crate::provider::ConnectOk, crate::provider::ConnectError> {
            Err(crate::provider::ConnectError::auth(
                "NO [AUTHENTICATIONFAILED]",
            ))
        }
    }

    fn sample_form() -> MailboxForm {
        MailboxForm {
            email: "you@example.com".into(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            imap_security: MailSecurity::Ssl,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            smtp_security: MailSecurity::StartTls,
        }
    }

    /// T-078: the sync worker lives outside this crate and has to reopen
    /// the very connection the wizard probed, so what comes back out must
    /// be what went in -- not defaults filled in by
    /// `load_account_row`'s `unwrap_or` arms.
    #[test]
    fn account_connection_returns_the_settings_the_account_was_saved_with() {
        let mut core = Core::memory().unwrap();
        let form = sample_form();
        let id = core
            .add_account(&form, "s3cret-pass", &OkConnector)
            .unwrap();

        let saved = core.account_connection(&id).unwrap();

        assert_eq!(saved.provider, "generic");
        assert_eq!(saved.form, form, "round-trip, not defaults");
    }

    /// D14: the password handed to `add_account` went to the keyring, and
    /// nothing on the way back out may carry it -- this is the type that
    /// crosses into `feathermail-service`, so its Debug is the one that
    /// would end up in a log line.
    #[test]
    fn account_connection_carries_no_secret() {
        let mut core = Core::memory().unwrap();
        let id = core
            .add_account(&sample_form(), "s3cret-pass", &OkConnector)
            .unwrap();

        let printed = format!("{:?}", core.account_connection(&id).unwrap());

        assert!(
            !printed.contains("s3cret-pass"),
            "AccountConnection's Debug leaked the password: {printed}"
        );
    }

    #[test]
    fn account_connection_for_an_unknown_account_is_account_not_found() {
        let core = Core::memory().unwrap();
        let err = core
            .account_connection(&AccountId("nobody".into()))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);
    }

    #[test]
    fn add_account_saves_without_secret() {
        let mut core = Core::memory().unwrap();
        let id = core
            .add_account(&sample_form(), "s3cret-pass", &OkConnector)
            .unwrap();
        assert_eq!(id.as_str(), "you");
        let conn = core.db.conn();
        let (email, provider, imap_host, imap_sec, user, status): (
            String,
            String,
            String,
            String,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT email, provider, imap_host, imap_security, username, status
                 FROM accounts WHERE id = 'you'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(email, "you@example.com");
        assert_eq!(provider, "generic");
        assert_eq!(imap_host, "imap.example.com");
        assert_eq!(imap_sec, "SSL");
        assert_eq!(user, "you@example.com");
        assert_eq!(status, "synced");
        let dump: String = conn
            .query_row(
                "SELECT printf('%s', group_concat(sql, ' ')) FROM sqlite_master",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!dump.to_ascii_lowercase().contains("password"));
        let values: String = conn
            .query_row(
                "SELECT quote(id)||quote(name)||quote(email)||quote(provider)
                      ||quote(imap_host)||quote(imap_port)||quote(smtp_host)||quote(smtp_port)
                      ||quote(imap_security)||quote(smtp_security)||quote(username)
                      ||quote(status)
                 FROM accounts WHERE id = 'you'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !values.contains("s3cret-pass"),
            "secret leaked into sqlite: {values}"
        );
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM folders WHERE account_id = 'you' AND kind = 'inbox'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn add_account_wrong_password_does_not_insert() {
        let mut core = Core::memory().unwrap();
        let err = core
            .add_account(&sample_form(), "nope", &AuthFailConnector)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AuthRequired);
        assert_eq!(err.message, "That password wasn't accepted.");
        assert!(!err.message.to_ascii_lowercase().contains("imap"));
        let n: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn add_account_duplicate_email_is_human() {
        let mut core = Core::memory().unwrap();
        core.add_account(&sample_form(), "s3cret-pass", &OkConnector)
            .unwrap();
        let err = core
            .add_account(&sample_form(), "s3cret-pass", &OkConnector)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.message, AddAccountError::Duplicate.as_str());
    }

    #[test]
    fn add_gmail_account_saves_without_token() {
        let mut core = Core::memory().unwrap();
        let id = core
            .add_gmail_account("you@gmail.com", "ya29.s3cret", &OkConnector)
            .unwrap();
        assert_eq!(id.as_str(), "you");
        let conn = core.db.conn();
        let (provider, imap_host, smtp_host, email): (String, String, String, String) = conn
            .query_row(
                "SELECT provider, imap_host, smtp_host, email FROM accounts WHERE id = 'you'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(provider, "gmail");
        assert_eq!(imap_host, "imap.gmail.com");
        assert_eq!(smtp_host, "smtp.gmail.com");
        assert_eq!(email, "you@gmail.com");
        let values: String = conn
            .query_row(
                "SELECT quote(id)||quote(name)||quote(email)||quote(provider)
                      ||quote(imap_host)||quote(imap_port)||quote(smtp_host)||quote(smtp_port)
                      ||quote(imap_security)||quote(smtp_security)||quote(username)
                      ||quote(status)
                 FROM accounts WHERE id = 'you'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !values.contains("ya29.s3cret"),
            "token leaked into sqlite: {values}"
        );
    }

    /// T-165: same mailbox as Gmail, different `provider` -- that string
    /// is the whole difference, because it is what tells the connector to
    /// ask the session's account manager for the next token instead of a
    /// Feather Mail OAuth client. If this ever saved "gmail", a GOA
    /// account would silently look for a Google refresh token that was
    /// never stored.
    #[test]
    fn add_goa_account_saves_the_gmail_mailbox_under_its_own_provider() {
        let mut core = Core::memory().unwrap();
        let id = core
            .add_goa_account("you@gmail.com", "ya29.s3cret", &OkConnector)
            .unwrap();
        let conn = core.db.conn();
        let (provider, imap_host, smtp_host, email): (String, String, String, String) = conn
            .query_row(
                "SELECT provider, imap_host, smtp_host, email FROM accounts WHERE id = 'you'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(provider, "goa");
        assert_eq!(imap_host, "imap.gmail.com");
        assert_eq!(smtp_host, "smtp.gmail.com");
        assert_eq!(email, "you@gmail.com");
        let values: String = conn
            .query_row(
                "SELECT quote(id)||quote(name)||quote(email)||quote(provider)
                      ||quote(imap_host)||quote(imap_port)||quote(smtp_host)||quote(smtp_port)
                      ||quote(imap_security)||quote(smtp_security)||quote(username)
                      ||quote(status)
                 FROM accounts WHERE id = 'you'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !values.contains("ya29.s3cret"),
            "token leaked into sqlite: {values}"
        );
        assert_eq!(id.as_str(), "you");
    }

    #[test]
    fn add_goa_account_without_a_token_is_refused() {
        let mut core = Core::memory().unwrap();
        let err = core
            .add_goa_account("you@gmail.com", "", &OkConnector)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AuthRequired);
    }

    #[test]
    fn add_gmail_revoked_token_does_not_insert() {
        let mut core = Core::memory().unwrap();
        struct Reauth;
        impl MailConnector for Reauth {
            fn probe(
                &self,
                _: &MailboxForm,
                _: &str,
            ) -> Result<crate::provider::ConnectOk, crate::provider::ConnectError> {
                Err(crate::provider::ConnectError::reauth("invalid_grant"))
            }
        }
        let err = core
            .add_gmail_account("you@gmail.com", "ya29.dead", &Reauth)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AuthRequired);
        assert_eq!(err.message, "Sign in again to continue.");
        let n: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    // --- T-021: remove_account -------------------------------------------------

    use feathermail_security::{
        MemorySecretStore, SecretError, SecretKey, SecretStore, SecretString,
    };

    /// One row per table returned by `tables_with_account_id`, so a table
    /// this helper forgets shows up as a false pass, not a false failure.
    fn seed_full_account(core: &Core, acc: &str) {
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, ?2, 'generic', 'synced', 'recent', 0, 0)",
            params![acc, format!("{acc}@example.com")],
        )
        .unwrap();
        let inbox = format!("{acc}:inbox");
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES (?1, ?2, 'Inbox', 'inbox')",
            params![inbox, acc],
        )
        .unwrap();
        let t1 = format!("{acc}:t1");
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
             VALUES (?1, ?2, ?3, 'Hello', 'Hi', 0, 1)",
            params![t1, acc, inbox],
        )
        .unwrap();
        let m1 = format!("{acc}:m1");
        conn.execute(
            "INSERT INTO messages (id, account_id, thread_id, folder_id, date)
             VALUES (?1, ?2, ?3, ?4, 0)",
            params![m1, acc, t1, inbox],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO attachments (id, account_id, message_id, filename) VALUES (?1, ?2, ?3, 'a.pdf')",
            params![format!("{acc}:att1"), acc, m1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO labels (id, account_id, name) VALUES (?1, ?2, 'Label')",
            params![format!("{acc}:l1"), acc],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO drafts (id, account_id, from_addr, updated_at) VALUES (?1, ?2, ?3, 0)",
            params![format!("{acc}:d1"), acc, format!("{acc}@example.com")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO draft_attachments
             (id, account_id, draft_id, filename, size_bytes, source_path)
             VALUES (?1, ?2, ?3, 'draft.txt', 1, '/tmp/draft.txt')",
            params![format!("{acc}:da1"), acc, format!("{acc}:d1")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO snoozes (id, account_id, thread_id, until_ts, previous_folder_id)
             VALUES (?1, ?2, ?3, 100, ?4)",
            params![format!("{acc}:snz1"), acc, t1, inbox],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sync_state (account_id, folder_id, uidnext) VALUES (?1, ?2, 1)",
            params![acc, inbox],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sync_requests (account_id, requested_at) VALUES (?1, 0)",
            params![acc],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO operations (id, account_id, target_id, op, payload_hash, created_at)
             VALUES (?1, ?2, ?3, 'archive', 'h', 0)",
            params![format!("{acc}:op1"), acc, t1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO outbox (id, account_id, to_addr, created_at) VALUES (?1, ?2, 'x@x.com', 0)",
            params![format!("{acc}:ob1"), acc],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mcp_audit (tool, account_id, outcome, created_at)
             VALUES ('list_accounts', ?1, 'ok', 0)",
            params![acc],
        )
        .unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO mcp_clients (id, name, enabled, permission_level, created_at)
             VALUES ('stdio', 'Local stdio', 1, 'draft', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mcp_confirmation_requests
             (client_id, capability, account_id, fingerprint, status, created_at, expires_at)
             VALUES ('stdio', 'send_draft', ?1, ?2, 'pending', 0, 120)",
            params![acc, format!("send:{acc}:1")],
        )
        .unwrap();
    }

    #[test]
    fn remove_account_wipes_only_that_account() {
        let mut core = Core::memory().unwrap();
        seed_full_account(&core, "john");
        seed_full_account(&core, "jane");

        let secrets = MemorySecretStore::new();
        secrets
            .put(&SecretKey::password("john"), "hunter2")
            .unwrap();
        secrets
            .put(&SecretKey::oauth_access("john"), "at-1")
            .unwrap();
        secrets
            .put(&SecretKey::password("jane"), "hunter3")
            .unwrap();

        core.remove_account(&AccountId("john".into()), &secrets)
            .unwrap();

        let accounts_left: Vec<String> = {
            let conn = core.db.conn();
            let mut stmt = conn.prepare("SELECT id FROM accounts ORDER BY id").unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(accounts_left, vec!["jane".to_string()]);
        let clients_left: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM mcp_clients WHERE id = 'stdio'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(clients_left, 1, "global MCP policy outlives an account");

        // T-021: no table with account_id is left an orphan, and account B
        // (jane) is untouched — computed off the same table list
        // remove_account itself uses, so a new table is covered for free.
        let tables = core.db.tables_with_account_id().unwrap();
        assert!(tables.contains(&"operations".to_string()));
        assert!(tables.contains(&"threads".to_string()));
        assert!(tables.contains(&"messages".to_string()));
        assert!(tables.contains(&"folders".to_string()));
        for table in &tables {
            let conn = core.db.conn();
            let john_left: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE account_id = 'john'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(john_left, 0, "table {table} still has john rows");
            let jane_left: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE account_id = 'jane'"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(jane_left, 1, "table {table} lost jane's row");
        }

        assert!(secrets.get(&SecretKey::password("john")).unwrap().is_none());
        assert!(secrets
            .get(&SecretKey::oauth_access("john"))
            .unwrap()
            .is_none());
        assert_eq!(
            secrets
                .get(&SecretKey::password("jane"))
                .unwrap()
                .unwrap()
                .expose(),
            "hunter3"
        );
    }

    #[test]
    fn remove_account_sweeps_messages_fts_but_not_other_accounts() {
        // T-021 follow-up: messages_fts (fts5) has no account_id column, so
        // it is not in tables_with_account_id()'s sweep list. remove_account
        // has its own rowid-mapped FTS deletion — this proves it only touches
        // the removed account's rows. Seed both FTS and its T-068 rowid map
        // directly because this test isolates the account-removal lifecycle.
        let mut core = Core::memory().unwrap();
        seed_full_account(&core, "john");
        seed_full_account(&core, "jane");
        {
            let conn = core.db.conn();
            conn.execute(
                "INSERT INTO messages_fts (sender, recipients, subject, body, attachment_names, labels, message_id)
                 VALUES ('John Sender', 'r', 'John subject', 'John body', '', '', 'john:m1')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO fts_message_rows (message_id, fts_rowid) VALUES ('john:m1', ?1)",
                params![conn.last_insert_rowid()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages_fts (sender, recipients, subject, body, attachment_names, labels, message_id)
                 VALUES ('Jane Sender', 'r', 'Jane subject', 'Jane body', '', '', 'jane:m1')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO fts_message_rows (message_id, fts_rowid) VALUES ('jane:m1', ?1)",
                params![conn.last_insert_rowid()],
            )
            .unwrap();
        }

        core.remove_account(&AccountId("john".into()), &MemorySecretStore::new())
            .unwrap();

        let conn = core.db.conn();
        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT message_id FROM messages_fts ORDER BY message_id")
                .unwrap();
            stmt.query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(remaining, vec!["jane:m1".to_string()]);
    }

    #[test]
    fn remove_account_unknown_is_account_not_found() {
        let mut core = Core::memory().unwrap();
        let err = core
            .remove_account(&AccountId("nope".into()), &MemorySecretStore::new())
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);
    }

    /// F2: `remove_account` must delete the cached body *file* for the
    /// removed account's messages, not just the `body_path` row -- and
    /// must leave another account's cached body alone. Behavioral: checks
    /// what is actually on disk afterward, not that some cleanup call
    /// merely appears in the source.
    #[test]
    fn remove_account_deletes_that_accounts_cached_body_files_but_not_other_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_full_account(&core, "john");
        seed_full_account(&core, "jane");

        let john_id = crate::model::MessageId("john:m1".into());
        let jane_id = crate::model::MessageId("jane:m1".into());
        core.store_body(&john_id, dir.path(), b"john's secret body")
            .unwrap();
        core.store_body(&jane_id, dir.path(), b"jane's secret body")
            .unwrap();

        let john_rel: String = core
            .db
            .conn()
            .query_row(
                "SELECT body_path FROM messages WHERE id = 'john:m1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let jane_rel: String = core
            .db
            .conn()
            .query_row(
                "SELECT body_path FROM messages WHERE id = 'jane:m1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let john_file = dir.path().join(&john_rel);
        let jane_file = dir.path().join(&jane_rel);
        assert!(john_file.exists());
        assert!(jane_file.exists());

        core.remove_account_in(
            &AccountId("john".into()),
            &MemorySecretStore::new(),
            dir.path(),
        )
        .unwrap();

        assert!(
            !john_file.exists(),
            "john's cached body file must be deleted along with the account"
        );
        assert!(
            jane_file.exists(),
            "jane's cached body file must survive john's removal"
        );
        assert_eq!(
            std::fs::read(&jane_file).unwrap(),
            b"jane's secret body",
            "jane's surviving file must be untouched, not truncated"
        );
    }

    /// F2: an account that never had any cached body must remove cleanly --
    /// zero cache files is not an error case.
    #[test]
    fn remove_account_with_no_cached_bodies_does_not_fail() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_full_account(&core, "john");

        core.remove_account_in(
            &AccountId("john".into()),
            &MemorySecretStore::new(),
            dir.path(),
        )
        .unwrap();

        let n: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    struct FailingSecrets;

    impl SecretStore for FailingSecrets {
        fn put(&self, _key: &SecretKey, _secret: &str) -> Result<(), SecretError> {
            Err(SecretError::unavailable())
        }
        fn get(&self, _key: &SecretKey) -> Result<Option<SecretString>, SecretError> {
            Err(SecretError::unavailable())
        }
        fn delete(&self, _key: &SecretKey) -> Result<(), SecretError> {
            Err(SecretError::unavailable())
        }
    }

    #[test]
    fn remove_account_keyring_unavailable_does_not_block_local_removal() {
        let mut core = Core::memory().unwrap();
        seed_full_account(&core, "john");
        let report = core
            .remove_account(&AccountId("john".into()), &FailingSecrets)
            .unwrap();
        let message = report.keyring_error.expect("keyring failure is reported");
        assert!(
            !message.to_ascii_lowercase().contains("hunter"),
            "keyring_error must never carry the secret: {message}"
        );
        let n: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    // --- T-021: update_account --------------------------------------------------

    fn seed_generic_account(core: &Core, id: &str) {
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (
                id, name, email, provider, imap_host, imap_port, smtp_host, smtp_port,
                imap_security, smtp_security, username, status, download_policy, created_at, updated_at
            ) VALUES (?1, 'Old Name', ?2, 'generic', 'imap.old.test', 993, 'smtp.old.test', 587,
                      'SSL', 'STARTTLS', ?2, 'synced', 'recent', 0, 0)",
            params![id, format!("{id}@example.com")],
        )
        .unwrap();
    }

    fn seed_oauth_account(core: &Core, id: &str, provider: &str) {
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (
                id, name, email, provider, imap_host, imap_port, smtp_host, smtp_port,
                imap_security, smtp_security, username, status, download_policy, created_at, updated_at
            ) VALUES (?1, 'Old Name', ?2, ?3, 'imap.provider.test', 993, 'smtp.provider.test', 587,
                      'SSL', 'STARTTLS', ?2, 'synced', 'recent', 0, 0)",
            params![id, format!("{id}@example.com"), provider],
        )
        .unwrap();
    }

    fn account_row(core: &Core, id: &str) -> (String, String, u16, String) {
        core.db
            .conn()
            .query_row(
                "SELECT name, imap_host, imap_port, email FROM accounts WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get::<_, i64>(2)? as u16,
                        row.get(3)?,
                    ))
                },
            )
            .unwrap()
    }

    #[test]
    fn update_account_display_name_only_does_not_probe() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        struct PanicsIfProbed;
        impl MailConnector for PanicsIfProbed {
            fn probe(
                &self,
                _: &MailboxForm,
                _: &str,
            ) -> Result<crate::provider::ConnectOk, crate::provider::ConnectError> {
                panic!("display-name-only edit must not probe the network");
            }
        }
        let edit = AccountEdit {
            display_name: Some("New Name".into()),
            ..Default::default()
        };
        core.update_account(&AccountId("you".into()), &edit, &PanicsIfProbed)
            .unwrap();
        let (name, _, _, email) = account_row(&core, "you");
        assert_eq!(name, "New Name");
        assert_eq!(email, "you@example.com", "email is not editable");
    }

    #[test]
    fn update_account_failed_probe_leaves_row_unchanged() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let edit = AccountEdit {
            imap_host: Some("imap.new.test".into()),
            new_password: Some("new-secret".into()),
            ..Default::default()
        };
        let err = core
            .update_account(&AccountId("you".into()), &edit, &AuthFailConnector)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AuthRequired);
        let (name, imap_host, imap_port, _) = account_row(&core, "you");
        assert_eq!(name, "Old Name");
        assert_eq!(imap_host, "imap.old.test");
        assert_eq!(imap_port, 993);
    }

    #[test]
    fn update_account_generic_edits_host_after_successful_probe() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let edit = AccountEdit {
            display_name: Some("New Name".into()),
            imap_host: Some("imap.new.test".into()),
            imap_port: Some(994),
            new_password: Some("new-secret".into()),
            ..Default::default()
        };
        core.update_account(&AccountId("you".into()), &edit, &OkConnector)
            .unwrap();
        let (name, imap_host, imap_port, email) = account_row(&core, "you");
        assert_eq!(name, "New Name");
        assert_eq!(imap_host, "imap.new.test");
        assert_eq!(imap_port, 994);
        assert_eq!(email, "you@example.com");
    }

    #[test]
    fn update_account_oauth_cannot_edit_hosts() {
        let mut core = Core::memory().unwrap();
        seed_oauth_account(&core, "you", "gmail");
        let edit = AccountEdit {
            imap_host: Some("evil.test".into()),
            new_password: Some("ya29.whatever".into()),
            ..Default::default()
        };
        let err = core
            .update_account(&AccountId("you".into()), &edit, &OkConnector)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        let (_, imap_host, _, _) = account_row(&core, "you");
        assert_eq!(imap_host, "imap.provider.test");
    }

    #[test]
    fn update_account_oauth_can_still_rename() {
        let mut core = Core::memory().unwrap();
        seed_oauth_account(&core, "you", "microsoft");
        let edit = AccountEdit {
            display_name: Some("Work Mail".into()),
            ..Default::default()
        };
        core.update_account(&AccountId("you".into()), &edit, &OkConnector)
            .unwrap();
        let (name, _, _, _) = account_row(&core, "you");
        assert_eq!(name, "Work Mail");
    }

    #[test]
    fn update_account_unknown_is_account_not_found() {
        let mut core = Core::memory().unwrap();
        let edit = AccountEdit {
            display_name: Some("Ghost".into()),
            ..Default::default()
        };
        let err = core
            .update_account(&AccountId("nope".into()), &edit, &OkConnector)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);
    }

    #[test]
    fn update_account_host_edit_needs_a_password_to_probe_with() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let edit = AccountEdit {
            imap_host: Some("imap.new.test".into()),
            ..Default::default()
        };
        let err = core
            .update_account(&AccountId("you".into()), &edit, &OkConnector)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        let (_, imap_host, _, _) = account_row(&core, "you");
        assert_eq!(imap_host, "imap.old.test");
    }

    // --- T-074: list_accounts ----------------------------------------------------

    #[test]
    fn recipient_typeahead_remembers_a_successfully_sent_outbox_address() {
        let core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        core.db
            .conn()
            .execute(
                "INSERT INTO outbox
                 (id, account_id, to_addr, cc, bcc, created_at, status)
                 VALUES ('sent:1', 'you', 'Maya Chen <maya@northstar.test>',
                         '', '', 100, 'sent')",
                [],
            )
            .unwrap();

        let suggestions = core
            .suggest_addresses(&AccountId("you".into()), "north", 8)
            .unwrap();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].email, "maya@northstar.test");
    }

    #[test]
    fn list_accounts_is_empty_on_a_fresh_profile() {
        let core = Core::memory().unwrap();
        assert_eq!(core.list_accounts().unwrap(), Vec::new());
    }

    #[test]
    fn list_accounts_orders_oldest_first_and_reads_status() {
        let mut core = Core::memory().unwrap();
        core.set_now(50);
        core.add_account(&sample_form(), "s3cret-pass", &OkConnector)
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
                 VALUES ('zeta', 'Zeta', 'zeta@example.com', 'generic', 'error', 'recent', 100, 100)",
                [],
            )
            .unwrap();
        let accounts = core.list_accounts().unwrap();
        assert_eq!(
            accounts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["you", "zeta"],
            "oldest created_at first"
        );
        assert_eq!(accounts[0].status, AccountStatus::Synced);
        assert_eq!(accounts[1].status, AccountStatus::Error);
        assert_eq!(accounts[1].email, "zeta@example.com");
    }

    #[test]
    fn list_accounts_unknown_status_text_reads_back_as_offline() {
        let core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        core.db
            .conn()
            .execute(
                "UPDATE accounts SET status = 'garbled' WHERE id = 'you'",
                [],
            )
            .unwrap();
        let accounts = core.list_accounts().unwrap();
        assert_eq!(accounts[0].status, AccountStatus::Offline);
    }

    // --- T-074: list_folders -------------------------------------------------------

    #[test]
    fn list_folders_unknown_account_is_account_not_found() {
        let core = Core::memory().unwrap();
        let err = core.list_folders(&AccountId("nope".into())).unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);
    }

    #[test]
    fn list_folders_order_is_system_then_custom_and_counts_start_from_seeded_threads() {
        let core = Core::memory().unwrap();
        seed(&core);
        let folders = core.list_folders(&john()).unwrap();
        let kinds: Vec<FolderKind> = folders.iter().map(|f| f.folder.kind).collect();
        assert_eq!(
            kinds,
            vec![
                FolderKind::Inbox,
                FolderKind::Starred,
                FolderKind::Snoozed,
                FolderKind::Sent,
                FolderKind::Drafts,
                FolderKind::Archive,
                FolderKind::Spam,
                FolderKind::Trash,
                FolderKind::Custom,
            ],
            "system folders before custom, in a fixed sidebar order"
        );
        let inbox = &folders[0];
        assert_eq!(inbox.folder.id.as_str(), "inbox");
        assert_eq!(inbox.folder.label, "Inbox");
        assert_eq!(inbox.unread, 2, "both seeded threads are unread");
        assert_eq!(inbox.total, 2);
        // Sent/Drafts/Spam have no real folder row yet (nothing has synced)
        // but still appear, as zero-count placeholders, not missing rows.
        let sent = &folders[3];
        assert_eq!(sent.folder.kind, FolderKind::Sent);
        assert_eq!(sent.folder.id.as_str(), "john:sent");
        assert_eq!(sent.unread, 0);
        assert_eq!(sent.total, 0);
        let custom = folders.last().unwrap();
        assert_eq!(custom.folder.kind, FolderKind::Custom);
        assert_eq!(custom.folder.label, "Work");
        assert_eq!(custom.folder.account_id, Some(john()));
        assert_eq!(custom.unread, 0);
        assert_eq!(custom.total, 0);
    }

    #[test]
    fn list_folders_with_no_real_folders_yet_returns_placeholders() {
        let core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let folders = core.list_folders(&AccountId("you".into())).unwrap();
        assert_eq!(
            folders.len(),
            8,
            "no custom folders, and no real rows at all"
        );
        let inbox = &folders[0];
        assert_eq!(inbox.folder.kind, FolderKind::Inbox);
        assert_eq!(inbox.folder.id.as_str(), "you:inbox");
        assert_eq!(inbox.folder.label, "Inbox");
        assert_eq!(inbox.unread, 0);
        assert_eq!(inbox.total, 0);
    }

    /// D11: a folder's unread/total must always equal what `list_threads`
    /// would show for that folder — proved here across a real mutation
    /// (archiving marks the thread read, per `apply_one`'s Archive SQL).
    #[test]
    fn list_folders_counts_match_real_content_after_archive_dispatch() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);

        let before = core.list_folders(&john()).unwrap();
        let inbox_before = before
            .iter()
            .find(|f| f.folder.kind == FolderKind::Inbox)
            .unwrap();
        assert_eq!(inbox_before.unread, 2);
        assert_eq!(inbox_before.total, 2);
        let archive_before = before
            .iter()
            .find(|f| f.folder.kind == FolderKind::Archive)
            .unwrap();
        assert_eq!(archive_before.total, 0);

        core.dispatch(Command::Archive {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        })
        .unwrap();

        let after = core.list_folders(&john()).unwrap();
        let inbox_after = after
            .iter()
            .find(|f| f.folder.kind == FolderKind::Inbox)
            .unwrap();
        assert_eq!(inbox_after.unread, 1, "t1 left the inbox");
        assert_eq!(inbox_after.total, 1);
        let archive_after = after
            .iter()
            .find(|f| f.folder.kind == FolderKind::Archive)
            .unwrap();
        assert_eq!(archive_after.total, 1, "t1 landed in archive");
        assert_eq!(
            archive_after.unread, 0,
            "archiving also marks the thread read (apply_one)"
        );
        // t2 is untouched and still matches list_threads for inbox directly.
        let inbox_page = core
            .list_threads(ListThreadsQuery {
                account_id: john(),
                folder_id: FolderId("inbox".into()),
                filter: ThreadFilter::All,
                after: None,
                limit: 64,
            })
            .unwrap();
        assert_eq!(inbox_page.total as u32, inbox_after.total);
    }

    // --- T-076/T-077: a real Archive/Trash folder replaces the overlay -------------

    /// Before any real folder discovery, `list_folders` still shows the
    /// `archive`/`trash` flag overlays (there is nothing else to show
    /// them from) alongside the always-on `starred`/`snoozed` overlays.
    #[test]
    fn list_folders_overlays_archive_and_trash_when_no_real_folder_exists_yet() {
        let core = Core::memory().unwrap();
        seed(&core);
        let folders = core.list_folders(&john()).unwrap();
        let archive = folders
            .iter()
            .find(|f| f.folder.kind == FolderKind::Archive)
            .unwrap();
        assert_eq!(archive.folder.id.as_str(), "archive");
        let trash = folders
            .iter()
            .find(|f| f.folder.kind == FolderKind::Trash)
            .unwrap();
        assert_eq!(trash.folder.id.as_str(), "trash");
        assert!(folders.iter().any(|f| f.folder.kind == FolderKind::Starred));
        assert!(folders.iter().any(|f| f.folder.kind == FolderKind::Snoozed));
    }

    /// T-076: once a real `folders` row of kind Archive/Trash exists
    /// (discovered from the server via `Core::sync_folders`, T-077), the
    /// flag overlay for that kind is suppressed — the real row wins and
    /// the sidebar shows exactly one Archive and one Trash, not two.
    /// Starred/Snoozed remain overlays regardless, since no provider ever
    /// discovers those kinds.
    #[test]
    fn list_folders_prefers_the_real_folder_over_the_archive_and_trash_overlay() {
        let core = Core::memory().unwrap();
        seed(&core);
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, remote_id, name, kind) \
                 VALUES ('john:archive', 'john', 'Archive', 'Archive', 'archive')",
                [],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, remote_id, name, kind) \
                 VALUES ('john:trash', 'john', 'Trash', 'Trash', 'trash')",
                [],
            )
            .unwrap();

        let folders = core.list_folders(&john()).unwrap();
        let archives: Vec<_> = folders
            .iter()
            .filter(|f| f.folder.kind == FolderKind::Archive)
            .collect();
        assert_eq!(archives.len(), 1, "exactly one Archive row, not two");
        assert_eq!(
            archives[0].folder.id.as_str(),
            "john:archive",
            "the real folder wins, not the overlay's synthesized id"
        );
        let trashes: Vec<_> = folders
            .iter()
            .filter(|f| f.folder.kind == FolderKind::Trash)
            .collect();
        assert_eq!(trashes.len(), 1, "exactly one Trash row, not two");
        assert_eq!(trashes[0].folder.id.as_str(), "john:trash");

        // Starred/Snoozed are unaffected: still exactly one overlay each.
        assert_eq!(
            folders
                .iter()
                .filter(|f| f.folder.kind == FolderKind::Starred)
                .count(),
            1
        );
        assert_eq!(
            folders
                .iter()
                .filter(|f| f.folder.kind == FolderKind::Snoozed)
                .count(),
            1
        );
    }

    #[test]
    fn undo_before_ack_cancels_operation_and_restores_optimistic_state() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(FIXTURE_NOW);
        let receipt = core
            .dispatch_with_receipt(Command::Archive {
                account_id: john(),
                thread_ids: vec![ThreadId("t1".into())],
            })
            .unwrap();
        let operation = receipt.operations.first().unwrap();
        assert!(matches!(
            core.get_thread(&john(), &ThreadId("t1".into()))
                .unwrap()
                .placement,
            Placement::Archived { .. }
        ));
        let result = core.undo(&operation.undo_ticket()).unwrap();
        assert_eq!(
            result,
            UndoReceipt::Cancelled {
                operation_id: operation.operation_id.clone()
            }
        );
        assert!(matches!(
            core.get_thread(&john(), &ThreadId("t1".into()))
                .unwrap()
                .placement,
            Placement::Active { .. }
        ));
        let (status, undo_requested_at): (String, Option<i64>) = core
            .db
            .conn()
            .query_row(
                "SELECT status, undo_requested_at FROM operations WHERE id = ?1",
                params![operation.operation_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
        assert_eq!(undo_requested_at, Some(FIXTURE_NOW));
    }

    #[test]
    fn undo_after_ack_materializes_a_reverse_operation_with_causal_link() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(FIXTURE_NOW);
        let receipt = core
            .dispatch_with_receipt(Command::MarkRead {
                account_id: john(),
                thread_ids: vec![ThreadId("t1".into())],
            })
            .unwrap();
        let original = receipt.operations.first().unwrap().operation_id.clone();
        core.db
            .conn()
            .execute(
                "UPDATE operations SET status = 'acked' WHERE id = ?1",
                params![original.as_str()],
            )
            .unwrap();
        let reverse = core
            .undo(&UndoTicket {
                operation_id: original.clone(),
            })
            .unwrap();
        let reverse_id = match reverse {
            UndoReceipt::ReverseQueued {
                reverse_operation_id,
                ..
            } => reverse_operation_id,
            other => panic!("expected reverse operation, got {other:?}"),
        };
        let (status, undo_of, op): (String, Option<String>, String) = core
            .db
            .conn()
            .query_row(
                "SELECT status, undo_of, op FROM operations WHERE id = ?1",
                params![reverse_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(undo_of.as_deref(), Some(original.as_str()));
        assert_eq!(op, "mark_unread");
    }

    #[test]
    fn restore_trashed_thread_cancels_its_pending_trash_and_restores_exact_local_state() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(FIXTURE_NOW);
        let receipt = core
            .dispatch_with_receipt(Command::Trash {
                account_id: john(),
                thread_ids: vec![tid("t1")],
            })
            .unwrap();
        let original = receipt.operations[0].operation_id.clone();

        let restored = core.restore_trashed_thread(&john(), &tid("t1")).unwrap();

        assert_eq!(
            restored,
            UndoReceipt::Cancelled {
                operation_id: original.clone()
            }
        );
        assert!(matches!(
            core.get_thread(&john(), &tid("t1")).unwrap().placement,
            Placement::Active { .. }
        ));
        let (status, undo_requested_at): (String, Option<i64>) = core
            .db
            .conn()
            .query_row(
                "SELECT status, undo_requested_at FROM operations WHERE id = ?1",
                params![original.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
        assert_eq!(undo_requested_at, Some(FIXTURE_NOW));
        assert_eq!(core.queue_counts().unwrap().pending, 0);
    }

    #[test]
    fn restore_trashed_thread_after_ack_queues_the_existing_causal_move() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(FIXTURE_NOW);
        let receipt = core
            .dispatch_with_receipt(Command::Trash {
                account_id: john(),
                thread_ids: vec![tid("t1")],
            })
            .unwrap();
        let original = receipt.operations[0].operation_id.clone();
        core.db
            .conn()
            .execute(
                "UPDATE operations SET status = 'acked' WHERE id = ?1",
                params![original.as_str()],
            )
            .unwrap();

        let reverse = core.restore_trashed_thread(&john(), &tid("t1")).unwrap();

        let reverse_id = match reverse {
            UndoReceipt::ReverseQueued {
                operation_id,
                reverse_operation_id,
            } => {
                assert_eq!(operation_id, original);
                reverse_operation_id
            }
            other => panic!("expected causal reverse, got {other:?}"),
        };
        assert!(matches!(
            core.get_thread(&john(), &tid("t1")).unwrap().placement,
            Placement::Active { .. }
        ));
        let (status, undo_of, op): (String, Option<String>, String) = core
            .db
            .conn()
            .query_row(
                "SELECT status, undo_of, op FROM operations WHERE id = ?1",
                params![reverse_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "pending");
        assert_eq!(undo_of.as_deref(), Some(original.as_str()));
        assert_eq!(op, "move");
    }

    #[test]
    fn restore_trashed_thread_also_recognizes_a_real_trash_folder() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(FIXTURE_NOW);
        core.db
            .conn()
            .execute(
                "UPDATE folders SET remote_id = 'INBOX' WHERE id = 'inbox'",
                [],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, remote_id, name, kind)
                 VALUES ('trash', 'john', 'Trash', 'Trash', 'trash')",
                [],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO messages (
                    id, account_id, thread_id, folder_id, provider_uid,
                    date, sender_name, sender_email, subject, snippet, unread
                 ) VALUES ('m1', 'john', 't1', 'inbox', 7, ?1,
                           'Sender', 'sender@example.com', 'Hello', 'Hello', 1)",
                params![FIXTURE_NOW],
            )
            .unwrap();
        core.dispatch(Command::Trash {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        })
        .unwrap();
        let moved = core.get_thread(&john(), &tid("t1")).unwrap();
        assert_eq!(moved.folder.as_str(), "trash");
        assert!(!moved.deleted());

        let restored = core.restore_trashed_thread(&john(), &tid("t1")).unwrap();

        assert!(matches!(restored, UndoReceipt::Cancelled { .. }));
        let thread = core.get_thread(&john(), &tid("t1")).unwrap();
        assert_eq!(thread.folder.as_str(), "inbox");
        assert!(!thread.deleted());
        assert_eq!(core.queue_counts().unwrap().pending, 0);
    }

    #[test]
    fn restore_trashed_thread_refuses_unknown_non_trashed_or_superseded_state() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        assert_eq!(
            core.restore_trashed_thread(&john(), &tid("missing"))
                .unwrap_err()
                .code,
            ErrorCode::MessageNotFound
        );
        assert_eq!(
            core.restore_trashed_thread(&john(), &tid("t1"))
                .unwrap_err()
                .code,
            ErrorCode::OperationNotSupported
        );

        core.dispatch(Command::Trash {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        })
        .unwrap();
        core.dispatch(Command::PermanentDelete {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        })
        .unwrap();

        assert_eq!(
            core.restore_trashed_thread(&john(), &tid("t1"))
                .unwrap_err()
                .code,
            ErrorCode::OperationNotSupported
        );
        assert!(matches!(
            core.get_thread(&john(), &tid("t1")).unwrap().placement,
            Placement::Trashed
        ));
        assert_eq!(core.queue_counts().unwrap().pending, 2);
    }

    #[test]
    fn restore_trashed_thread_waits_for_a_competing_placement_change_before_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let mut setup = Core::open(&path).unwrap();
            setup.set_now(FIXTURE_NOW);
            seed(&setup);
            setup
                .dispatch(Command::Trash {
                    account_id: john(),
                    thread_ids: vec![tid("t1")],
                })
                .unwrap();
        }
        let mut restoring = Core::open(&path).unwrap();
        restoring.set_now(FIXTURE_NOW + 1);
        let mut competing = Core::open(&path).unwrap();
        competing.set_now(FIXTURE_NOW + 1);
        let tx = competing.db.immediate_transaction().unwrap();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            result_tx
                .send(restoring.restore_trashed_thread(&john(), &tid("t1")))
                .unwrap();
        });

        assert!(result_rx.recv_timeout(Duration::from_millis(100)).is_err());
        dispatch_with_receipt_in(
            &tx,
            &Command::PermanentDelete {
                account_id: john(),
                thread_ids: vec![tid("t1")],
            },
            FIXTURE_NOW + 1,
        )
        .unwrap();
        tx.commit().unwrap();

        assert_eq!(
            result_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .code,
            ErrorCode::OperationNotSupported
        );
        worker.join().unwrap();
        let (deleted, pending): (i64, i64) = competing
            .db
            .conn()
            .query_row(
                "SELECT t.deleted,
                        (SELECT COUNT(*) FROM operations
                         WHERE account_id = 'john' AND target_id = 't1'
                           AND status = 'pending')
                 FROM threads t WHERE t.account_id = 'john' AND t.id = 't1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(pending, 2);
    }

    #[test]
    fn permanent_delete_has_no_undo_ticket() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        let receipt = core
            .dispatch_with_receipt(Command::PermanentDelete {
                account_id: john(),
                thread_ids: vec![ThreadId("t1".into())],
            })
            .unwrap();
        let ticket = receipt.operations.first().unwrap().undo_ticket();
        assert_eq!(
            core.undo(&ticket).unwrap_err().code,
            ErrorCode::OperationNotSupported
        );
    }

    #[test]
    fn vector_star_updates_only_selected_threads_and_returns_each_receipt() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        let selected = vec![ThreadId("t1".into()), ThreadId("t2".into())];

        let receipt = core
            .dispatch_with_receipt(Command::Star {
                account_id: john(),
                thread_ids: selected.clone(),
            })
            .unwrap();

        assert_eq!(receipt.operations.len(), selected.len());
        assert_eq!(
            receipt
                .operations
                .iter()
                .map(|operation| operation.thread_id.clone())
                .collect::<Vec<_>>(),
            selected
        );
        assert!(
            core.get_thread(&john(), &ThreadId("t1".into()))
                .unwrap()
                .starred
        );
        assert!(
            core.get_thread(&john(), &ThreadId("t2".into()))
                .unwrap()
                .starred
        );
        assert_eq!(core.queue_counts().unwrap().pending, 2);
    }

    #[test]
    fn vector_unstar_updates_only_selected_threads_and_returns_each_receipt() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.db
            .conn()
            .execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
                 VALUES ('t3', 'john', 'inbox', 'Untouched', 'Third', ?1, 1, 1)",
                params![FIXTURE_NOW - 120],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "UPDATE threads SET starred = 1 WHERE account_id = 'john' AND id IN ('t1', 't2')",
                [],
            )
            .unwrap();
        let selected = vec![ThreadId("t1".into()), ThreadId("t2".into())];

        let receipt = core
            .dispatch_with_receipt(Command::Unstar {
                account_id: john(),
                thread_ids: selected.clone(),
            })
            .unwrap();

        assert_eq!(receipt.operations.len(), selected.len());
        assert_eq!(
            receipt
                .operations
                .iter()
                .map(|operation| operation.thread_id.clone())
                .collect::<Vec<_>>(),
            selected
        );
        assert!(selected
            .iter()
            .all(|thread_id| { !core.get_thread(&john(), thread_id).unwrap().starred }));
        assert!(
            core.get_thread(&john(), &ThreadId("t3".into()))
                .unwrap()
                .starred
        );
        assert_eq!(core.queue_counts().unwrap().pending, 2);
    }

    // --- T-074: create_folder ------------------------------------------------------

    /// T-060s: the headless door is durable and collapses repeats. Two
    /// requests a second apart are one sync, and the row survives until a
    /// shell actually claims it -- a request made while nothing is running
    /// is not a request that is lost.
    #[test]
    fn request_account_sync_collapses_onto_one_durable_row() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        seed_generic_account(&core, "work");
        let you = AccountId("you".into());
        core.request_account_sync(&you).unwrap();
        core.set_now(FIXTURE_NOW + 1);
        core.request_account_sync(&you).unwrap();
        core.request_account_sync(&AccountId("work".into()))
            .unwrap();
        let (rows, requested_at): (i64, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*), MAX(requested_at) FROM sync_requests WHERE account_id = ?1",
                params!["you"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, 1, "one account, one pending sync");
        assert_eq!(requested_at, FIXTURE_NOW + 1, "the later ask wins");
        let total: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM sync_requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 2, "the second account keeps its own row");
    }

    #[test]
    fn request_account_sync_rejects_an_unknown_account() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let err = core
            .request_account_sync(&AccountId("nobody".into()))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);
        let total: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM sync_requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 0);
    }

    /// The shell polls twice a second. A claim that left the row behind
    /// would turn one `sync_account` call into an endless wake loop, so the
    /// claim has to consume what it returns.
    #[test]
    fn take_sync_requests_claims_each_request_exactly_once() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        seed_generic_account(&core, "work");
        core.request_account_sync(&AccountId("work".into()))
            .unwrap();
        core.set_now(FIXTURE_NOW + 5);
        core.request_account_sync(&AccountId("you".into())).unwrap();
        let first = core.take_sync_requests(1).unwrap();
        assert_eq!(
            first.iter().map(AccountId::as_str).collect::<Vec<_>>(),
            vec!["work"],
            "oldest request first"
        );
        let second = core.take_sync_requests(8).unwrap();
        assert_eq!(
            second.iter().map(AccountId::as_str).collect::<Vec<_>>(),
            vec!["you"]
        );
        assert!(core.take_sync_requests(8).unwrap().is_empty());
    }

    /// Removing an account must not leave a request that names it: the
    /// shell would claim an id it can no longer sync.
    #[test]
    fn removing_an_account_drops_its_pending_sync_request() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        core.request_account_sync(&AccountId("you".into())).unwrap();
        core.db
            .conn()
            .execute("DELETE FROM accounts WHERE id = ?1", params!["you"])
            .unwrap();
        assert!(core.take_sync_requests(8).unwrap().is_empty());
    }

    fn seed_named_folder(core: &Core, id: &str, remote: Option<&str>, name: &str, kind: &str) {
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, remote_id, name, kind, delimiter)
                 VALUES (?1, 'you', ?2, ?3, ?4, '/')",
                params![id, remote, name, kind],
            )
            .unwrap();
    }

    #[test]
    fn rename_folder_refuses_names_it_would_never_accept_for_a_new_folder() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        seed_named_folder(&core, "you:ideas", Some("Ideas"), "Ideas", "custom");
        let account = AccountId("you".into());
        let folder = FolderId("you:ideas".into());
        for (name, expected) in [
            ("   ".to_string(), RenameFolderError::Empty),
            ("Trash".into(), RenameFolderError::SystemName),
            ("bad\0name".into(), RenameFolderError::InvalidName),
            ("x".repeat(5000), RenameFolderError::InvalidName),
        ] {
            let err = core.rename_folder(&account, &folder, &name).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgument);
            assert_eq!(err.message, expected.as_str());
        }
        let queued: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, 0, "a refused rename must queue nothing");
    }

    #[test]
    fn rename_folder_refuses_a_sibling_name_that_is_already_taken() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        seed_named_folder(&core, "you:ideas", Some("Ideas"), "Ideas", "custom");
        seed_named_folder(&core, "you:plans", Some("Plans"), "Plans", "custom");
        let err = core
            .rename_folder(
                &AccountId("you".into()),
                &FolderId("you:ideas".into()),
                "plans",
            )
            .unwrap_err();
        assert_eq!(err.message, RenameFolderError::Duplicate.as_str());
    }

    #[test]
    fn rename_folder_refuses_system_folders_and_local_only_folders() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        seed_named_folder(&core, "you:inbox", Some("INBOX"), "Inbox", "inbox");
        seed_named_folder(&core, "you:fresh", None, "Fresh", "custom");
        let account = AccountId("you".into());
        for (id, expected) in [
            ("you:inbox", RenameFolderError::NotCustom),
            ("you:fresh", RenameFolderError::NotOnServer),
        ] {
            let err = core
                .rename_folder(&account, &FolderId(id.into()), "Whatever")
                .unwrap_err();
            assert_eq!(err.message, expected.as_str(), "{id}");
        }
        let err = core
            .rename_folder(&account, &FolderId("you:ghost".into()), "Whatever")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        let err = core
            .rename_folder(
                &AccountId("nobody".into()),
                &FolderId("you:inbox".into()),
                "Whatever",
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);
    }

    #[test]
    fn renaming_a_folder_to_the_name_it_already_has_is_a_no_op() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        seed_named_folder(&core, "you:ideas", Some("Ideas"), "Ideas", "custom");
        let queued_now = core
            .rename_folder(
                &AccountId("you".into()),
                &FolderId("you:ideas".into()),
                "  Ideas  ",
            )
            .unwrap();
        assert!(
            !queued_now,
            "a rename to the current name has nothing to do"
        );
        let queued: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, 0, "RENAME x x is a server error, not a no-op");
    }

    /// The two halves of a rename path, as `rename_folder` splits and
    /// `mailbox_remote_id` rejoins them: only the last segment is replaced,
    /// so `Team/Sub/Ideas` -> `Team/Sub/Plans`.
    #[test]
    fn a_renamed_path_replaces_only_its_last_segment() {
        let rename = |remote_id: &str, delimiter: Option<&str>, label: &str| {
            let (parent, delim) = mailbox_parent(remote_id, delimiter);
            mailbox_remote_id(&parent, &delim, label)
        };
        assert_eq!(rename("Ideas", Some("/"), "Plans"), "Plans");
        assert_eq!(
            rename("Team/Sub/Ideas", Some("/"), "Plans"),
            "Team/Sub/Plans"
        );
        assert_eq!(rename("Team.Ideas", Some("."), "Plans"), "Team.Plans");
        // A path that starts at the delimiter keeps it: the prefix is
        // empty, the delimiter is not.
        assert_eq!(rename("/Team/Ideas", Some("/"), "Plans"), "/Team/Plans");
        // No delimiter reported means a flat namespace, not "guess one".
        assert_eq!(rename("Team/Ideas", None, "Plans"), "Plans");
        assert_eq!(rename("Team/Ideas", Some(""), "Plans"), "Plans");
    }

    /// T-158: the encoding happens on the leaf and only on the leaf. A
    /// parent path came back from `LIST` already in the server's encoding;
    /// running it through the encoder again escapes its `&` and names a
    /// mailbox nobody has.
    #[test]
    fn only_the_leaf_of_a_rename_path_is_encoded() {
        // «Проекты» / «Отчёты»: the parent goes on the wire byte for byte,
        // the leaf is encoded once.
        assert_eq!(
            mailbox_remote_id("&BB8EQAQ+BDUEOgRCBEs-", "/", "Отчёты"),
            "&BB8EQAQ+BDUEOgRCBEs-/&BB4EQgRHBFEEQgRL-"
        );
        assert!(
            !mailbox_remote_id("&BB8EQAQ+BDUEOgRCBEs-", "/", "Отчёты").contains("&-"),
            "a `&-` in the result means the parent was escaped a second time"
        );
        // ASCII hierarchies are byte-identical to what they always were.
        assert_eq!(
            mailbox_remote_id("Team/Sub", "/", "Plans"),
            "Team/Sub/Plans"
        );
    }

    /// RFC 3501 §5.1.3, checked against the shapes the decoder in
    /// `feathermail_providers::folders` reads back.
    #[test]
    fn modified_utf7_encodes_non_ascii_and_escapes_the_ampersand() {
        assert_eq!(encode_modified_utf7("Notes"), "Notes");
        assert_eq!(encode_modified_utf7("Проекты"), "&BB8EQAQ+BDUEOgRCBEs-");
        assert_eq!(
            encode_modified_utf7("Исходящие"),
            "&BBgEQQRFBD4ENARPBEkEOAQ1-"
        );
        assert_eq!(encode_modified_utf7("R&D"), "R&-D");
        assert_eq!(encode_modified_utf7(""), "");
        // The three base64 tail lengths: 3, 2 and 1 leftover bytes.
        assert_eq!(encode_modified_utf7("\u{4f60}\u{597d}"), "&T2BZfQ-");
        assert_eq!(encode_modified_utf7("\u{4f60}"), "&T2A-");
        assert_eq!(encode_modified_utf7("\u{1f600}"), "&2D3eAA-");
    }

    fn seed_thread_in_folder(core: &Core, id: &str, folder: &str) {
        core.db
            .conn()
            .execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
                 VALUES (?1, 'you', ?2, 'Subject', 'Snippet', ?3, 0)",
                params![id, folder, FIXTURE_NOW - 60],
            )
            .unwrap();
    }

    #[test]
    fn delete_folder_tombstones_the_row_and_queues_the_mailbox_name() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        seed_named_folder(&core, "you:ideas", Some("Team/Ideas"), "Ideas", "custom");
        let queued = core
            .delete_folder(&AccountId("you".into()), &FolderId("you:ideas".into()))
            .unwrap();
        assert!(queued);

        let (deleted_at, remote_id): (Option<i64>, Option<String>) = core
            .db
            .conn()
            .query_row(
                "SELECT deleted_at, remote_id FROM folders WHERE id = 'you:ideas'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(deleted_at, Some(FIXTURE_NOW), "the row survives (D-FK)");
        assert_eq!(
            remote_id.as_deref(),
            Some("Team/Ideas"),
            "remote_id must live until the wire ACK, so a LIST in between \
             adopts this row instead of inserting a second one"
        );
        let (op, payload): (String, String) = core
            .db
            .conn()
            .query_row(
                "SELECT op, payload FROM operations WHERE target_id = 'you:ideas'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(op, "delete_folder");
        assert!(
            payload.contains(r#""mailbox":"Team/Ideas""#),
            "the full path, not the leaf: {payload}"
        );
        assert!(payload.contains(r#""at":"#), "D29 dedup escape: {payload}");
    }

    #[test]
    fn delete_folder_hides_the_tombstoned_folder_from_the_sidebar() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        seed_named_folder(&core, "you:ideas", Some("Ideas"), "Ideas", "custom");
        let account = AccountId("you".into());
        assert!(core
            .list_folders(&account)
            .unwrap()
            .iter()
            .any(|f| f.folder.id.as_str() == "you:ideas"));
        core.delete_folder(&account, &FolderId("you:ideas".into()))
            .unwrap();
        assert!(
            !core
                .list_folders(&account)
                .unwrap()
                .iter()
                .any(|f| f.folder.id.as_str() == "you:ideas"),
            "a tombstone still has a row, but it is not a folder any more"
        );
    }

    #[test]
    fn delete_folder_refuses_a_folder_that_still_holds_mail() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        seed_named_folder(&core, "you:ideas", Some("Ideas"), "Ideas", "custom");
        seed_thread_in_folder(&core, "t-ideas", "you:ideas");
        let err = core
            .delete_folder(&AccountId("you".into()), &FolderId("you:ideas".into()))
            .unwrap_err();
        assert_eq!(err.message, DeleteFolderError::NotEmpty.as_str());
        let deleted_at: Option<i64> = core
            .db
            .conn()
            .query_row(
                "SELECT deleted_at FROM folders WHERE id = 'you:ideas'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(deleted_at.is_none(), "a refusal must change nothing");
    }

    #[test]
    fn delete_folder_refuses_system_folders_and_repeats_are_a_no_op() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        seed_named_folder(&core, "you:inbox", Some("INBOX"), "Inbox", "inbox");
        seed_named_folder(&core, "you:ideas", Some("Ideas"), "Ideas", "custom");
        let account = AccountId("you".into());
        let err = core
            .delete_folder(&account, &FolderId("you:inbox".into()))
            .unwrap_err();
        assert_eq!(err.message, DeleteFolderError::NotCustom.as_str());
        let err = core
            .delete_folder(&account, &FolderId("you:ghost".into()))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        let err = core
            .delete_folder(&AccountId("nobody".into()), &FolderId("you:ideas".into()))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);

        assert!(core
            .delete_folder(&account, &FolderId("you:ideas".into()))
            .unwrap());
        assert!(
            !core
                .delete_folder(&account, &FolderId("you:ideas".into()))
                .unwrap(),
            "deleting an already-deleted folder queues nothing a second time"
        );
        let queued: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE op = 'delete_folder'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(queued, 1);
    }

    /// A folder created offline and deleted before the queue drained never
    /// existed on the server. Queueing a `DELETE` for it would be a `DELETE`
    /// with no mailbox name; leaving the `CREATE` in the queue would put the
    /// mailbox on the server *after* the user deleted it. Both are wrong, so
    /// the `CREATE` is cancelled and nothing is queued.
    #[test]
    fn deleting_a_folder_that_never_reached_the_server_cancels_its_create() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        let account = AccountId("you".into());
        let id = core.create_folder(&account, "Ideas").unwrap();
        let queued = core.delete_folder(&account, &id).unwrap();
        assert!(!queued, "no mailbox on the server means nothing to delete");
        let status: String = core
            .db
            .conn()
            .query_row(
                "SELECT status FROM operations WHERE op = 'create_folder' AND target_id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
        let deletes: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE op = 'delete_folder'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(deletes, 0);
    }

    /// The natural follow-up to the previous test: the user changes their
    /// mind and makes the folder again. `unique_folder_id` derives the same
    /// id from the same name, so an `INSERT` would hit the primary key and a
    /// duplicate check would report a folder the user cannot see. Reviving
    /// the tombstone is the only answer that matches what they asked for.
    #[test]
    fn creating_a_folder_again_revives_its_tombstone_instead_of_failing() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        let account = AccountId("you".into());
        let id = core.create_folder(&account, "Ideas").unwrap();
        core.delete_folder(&account, &id).unwrap();
        core.set_now(FIXTURE_NOW + 60);
        let again = core.create_folder(&account, "Ideas").unwrap();
        assert_eq!(again.as_str(), id.as_str());
        let (deleted_at, remote_id): (Option<i64>, Option<String>) = core
            .db
            .conn()
            .query_row(
                "SELECT deleted_at, remote_id FROM folders WHERE id = ?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(deleted_at.is_none(), "the folder is back");
        assert!(
            remote_id.is_none(),
            "it has to be created on the server again before it has one"
        );
        assert!(core
            .list_folders(&account)
            .unwrap()
            .iter()
            .any(|f| f.folder.id.as_str() == id.as_str()));
        let pending: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM operations
                 WHERE op = 'create_folder' AND target_id = ?1 AND status = 'pending'",
                params![id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1, "the revived folder is queued for the server");
    }

    #[test]
    fn create_folder_inserts_row_and_queues_an_operation() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed_generic_account(&core, "you");
        let id = core
            .create_folder(&AccountId("you".into()), "Ideas")
            .unwrap();
        assert_eq!(id.as_str(), "you:ideas");
        let (name, kind, color): (String, String, Option<String>) = core
            .db
            .conn()
            .query_row(
                "SELECT name, kind, color FROM folders WHERE id = ?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, "Ideas");
        assert_eq!(kind, "custom");
        assert_eq!(color.as_deref(), Some("#47CC50"));
        let (op, target, status): (String, String, String) = core
            .db
            .conn()
            .query_row(
                "SELECT op, target_id, status FROM operations WHERE target_id = ?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(op, "create_folder");
        assert_eq!(target, id.as_str());
        assert_eq!(status, "pending");
    }

    /// T-097(11): the picker's colour reaches the row. Not a cosmetic
    /// test -- the dot is how the sidebar tells two folders apart, and a
    /// picker whose answer is dropped is worse than no picker.
    #[test]
    fn create_folder_with_color_stores_the_colour_that_was_picked() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let id = core
            .create_folder_with_color(&AccountId("you".into()), "Ideas", Some("#2DD2E0"))
            .unwrap();
        let folders = core.list_folders(&AccountId("you".into())).unwrap();
        let custom = folders
            .iter()
            .find(|f| f.folder.id == id)
            .expect("the folder that was just created");
        assert_eq!(custom.folder.color, Some("#2DD2E0"));
    }

    /// A colour outside the palette is not an error and not stored: Core
    /// owns the palette (D21), so the fallback is the same round-robin a
    /// caller with no picker at all gets.
    #[test]
    fn create_folder_with_color_falls_back_when_the_colour_is_not_ours() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let id = core
            .create_folder_with_color(&AccountId("you".into()), "Ideas", Some("#123456"))
            .unwrap();
        let color: Option<String> = core
            .db
            .conn()
            .query_row(
                "SELECT color FROM folders WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(color.as_deref(), Some(FOLDER_PALETTE[0]));
    }

    /// T-060u's tombstone revive meets T-097(11): recreating a deleted
    /// name is a create, so the colour the user just picked for it wins --
    /// and a revive with no pick keeps the colour the row already had
    /// rather than resetting it to green.
    #[test]
    fn reviving_a_deleted_folder_takes_the_new_colour_and_keeps_the_old_one() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let account = AccountId("you".into());
        let id = core
            .create_folder_with_color(&account, "Ideas", Some("#9451F4"))
            .unwrap();
        let color_now = |core: &Core| -> Option<String> {
            core.db
                .conn()
                .query_row(
                    "SELECT color FROM folders WHERE id = ?1",
                    params![id.as_str()],
                    |row| row.get(0),
                )
                .unwrap()
        };

        core.delete_folder(&account, &id).unwrap();
        core.create_folder(&account, "Ideas").unwrap();
        assert_eq!(color_now(&core).as_deref(), Some("#9451F4"));

        core.delete_folder(&account, &id).unwrap();
        core.create_folder_with_color(&account, "Ideas", Some("#FB954A"))
            .unwrap();
        assert_eq!(color_now(&core).as_deref(), Some("#FB954A"));
    }

    #[test]
    fn create_folder_shows_up_in_list_folders_with_zero_counts() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        core.create_folder(&AccountId("you".into()), "Ideas")
            .unwrap();
        let folders = core.list_folders(&AccountId("you".into())).unwrap();
        let custom = folders.last().unwrap();
        assert_eq!(custom.folder.kind, FolderKind::Custom);
        assert_eq!(custom.folder.label, "Ideas");
        assert_eq!(custom.unread, 0);
        assert_eq!(custom.total, 0);
        assert_eq!(custom.folder.color, Some("#47CC50"));
    }

    #[test]
    fn create_folder_rejects_empty_name() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let err = core
            .create_folder(&AccountId("you".into()), "   ")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.message, CreateFolderError::Empty.as_str());
    }

    #[test]
    fn create_folder_rejects_system_names_case_insensitively() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let err = core
            .create_folder(&AccountId("you".into()), "  Trash ")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.message, CreateFolderError::SystemName.as_str());
        let n: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM folders WHERE account_id = 'you'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn create_folder_rejects_duplicate_names_case_insensitively() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        core.create_folder(&AccountId("you".into()), "Ideas")
            .unwrap();
        let err = core
            .create_folder(&AccountId("you".into()), "ideas")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(err.message, CreateFolderError::Duplicate.as_str());
        let n: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM folders WHERE account_id = 'you' AND kind = 'custom'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn create_folder_rejects_a_name_imap_would_never_quote() {
        let mut core = Core::memory().unwrap();
        seed_generic_account(&core, "you");
        let account = AccountId("you".into());
        for name in ["folder\0name".to_string(), "a\nb".into(), "x".repeat(5000)] {
            let err = core.create_folder(&account, &name).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgument);
            assert_eq!(err.message, CreateFolderError::InvalidName.as_str());
        }
        let n: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM folders WHERE account_id = 'you'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "a doomed CREATE must not leave a local row");
        let queued: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, 0, "a doomed CREATE must not queue an operation");
    }

    #[test]
    fn create_folder_unknown_account_is_account_not_found() {
        let mut core = Core::memory().unwrap();
        let err = core
            .create_folder(&AccountId("nope".into()), "Ideas")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);
    }

    /// An outside caller (MCP passes `draft_id` straight through) can plant a
    /// draft whose numeric tail does not fit in an i64. That used to make the
    /// sequence query fail, `unwrap_or(1)` reused `draft:john:1`, and the
    /// upsert silently replaced the draft that already owned it.
    #[test]
    fn a_new_draft_never_lands_on_an_existing_id() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(10);
        let first = core
            .save_draft(&john(), None, draft_content("keep me"))
            .unwrap();
        assert_eq!(first.id.as_str(), "draft:john:1");
        core.save_draft(
            &john(),
            Some(&DraftId("draft:john:99999999999999999999".into())),
            draft_content("planted"),
        )
        .unwrap();
        let third = core
            .save_draft(&john(), None, draft_content("brand new"))
            .unwrap();
        assert_eq!(
            core.get_draft(&john(), &first.id).unwrap().body,
            "keep me",
            "the existing draft must not be overwritten"
        );
        assert_ne!(
            third.id, first.id,
            "a new draft must not be handed an id that already exists"
        );
    }

    /// The unnumbered ids Core and MCP really write (`send-email` digests,
    /// oversized planted tails) are skipped rather than saturating the CAST.
    #[test]
    fn draft_numbering_ignores_ids_that_are_not_plain_numbers() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(10);
        core.save_draft(&john(), None, draft_content("one"))
            .unwrap();
        for planted in [
            "draft:john:99999999999999999999",
            "draft:john:send-email:abc",
            "draft:john:7x",
        ] {
            core.save_draft(
                &john(),
                Some(&DraftId(planted.into())),
                draft_content("planted"),
            )
            .unwrap();
        }
        assert_eq!(next_draft_sequence(core.db.conn(), "john").unwrap(), 2);
        assert_eq!(
            core.save_draft(&john(), None, draft_content("two"))
                .unwrap()
                .id
                .as_str(),
            "draft:john:2"
        );
    }

    /// A folder name may hold the two characters JSON cares about, and the
    /// provider reads `folder_id` back with the mirror of `json_escape`.
    /// Unescaped, `acc:a\\b` arrives as `acc:ab` -- a *different*, possibly
    /// existing, mailbox.
    #[test]
    fn a_move_payload_survives_a_folder_id_that_needs_escaping() {
        for label in ["a\\b", "Q1 \"plan\""] {
            let mut core = Core::memory().unwrap();
            seed(&core);
            core.set_now(FIXTURE_NOW);
            let folder = core.create_folder(&john(), label).unwrap();
            core.dispatch_with_receipt(Command::Move {
                account_id: john(),
                thread_ids: vec![tid("t1")],
                folder_id: folder.clone(),
            })
            .unwrap();
            let payload: String = core
                .db
                .conn()
                .query_row(
                    "SELECT payload FROM operations WHERE op = 'move' AND target_id = 't1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                json_string_field(&payload, "folder_id").as_deref(),
                Some(folder.as_str()),
                "folder_id must survive the round trip through the queue payload: {payload}"
            );
        }
    }

    #[test]
    fn a_draft_already_queued_for_sending_can_still_be_deleted() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(10);
        let draft = core.save_draft(&john(), None, draft_content("hi")).unwrap();
        core.queue_draft_send(&john(), &draft.id).unwrap();
        let result = core.delete_draft(&john(), &draft.id);
        assert!(
            result.is_ok(),
            "deleting a queued draft must not fail on a foreign key: {:?}",
            result.err()
        );
        assert_eq!(
            core.get_draft(&john(), &draft.id).unwrap_err().code,
            ErrorCode::MessageNotFound
        );
    }

    /// `threads.folder_id` has a foreign key to `folders(id)`, which proves
    /// the row exists but says nothing about whose account it is in. A thread
    /// parked in another account's folder is invisible in both.
    #[test]
    fn move_refuses_a_folder_that_belongs_to_another_account() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        seed_second_account(&core);
        core.set_now(FIXTURE_NOW);
        let outcome = core.dispatch(Command::Move {
            account_id: john(),
            thread_ids: vec![tid("t1")],
            folder_id: FolderId("jane:inbox".into()),
        });
        assert_eq!(outcome.unwrap_err().code, ErrorCode::InvalidArgument);
        let folder: String = core
            .db
            .conn()
            .query_row("SELECT folder_id FROM threads WHERE id = 't1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            folder, "inbox",
            "a thread must not move into another account's folder"
        );
    }

    /// And a folder id that names nothing at all is a bad argument, not the
    /// raw foreign-key failure ("Couldn't save that change.") it used to be.
    #[test]
    fn move_to_an_unknown_folder_is_an_invalid_argument() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(FIXTURE_NOW);
        let err = core
            .dispatch(Command::Move {
                account_id: john(),
                thread_ids: vec![tid("t1")],
                folder_id: FolderId("john:nope".into()),
            })
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
    }

    /// The snapshot `permanent_delete_payload` freezes is what makes the
    /// EXPUNGE match what the user saw; `locator` covers reading it back.
    #[test]
    fn permanent_delete_records_the_targets_it_was_dispatched_with() {
        let mut core = Core::memory().unwrap();
        seed(&core);
        core.set_now(FIXTURE_NOW);
        core.db
            .conn()
            .execute(
                "UPDATE folders SET remote_id = 'INBOX' WHERE id = 'inbox'",
                [],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO messages (id, account_id, thread_id, folder_id, date, provider_uid)
                 VALUES ('m1', 'john', 't1', 'inbox', 0, 7)",
                [],
            )
            .unwrap();
        core.dispatch(Command::PermanentDelete {
            account_id: john(),
            thread_ids: vec![tid("t1")],
        })
        .unwrap();
        let payload: String = core
            .db
            .conn()
            .query_row(
                "SELECT payload FROM operations WHERE op = 'permanent_delete' AND target_id = 't1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            payload, r#"{"targets":[{"folder":"INBOX","uid":7}]}"#,
            "the queued operation must carry its own target snapshot"
        );
    }
}
