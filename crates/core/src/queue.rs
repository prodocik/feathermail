//! Operation queue: claim, provider apply, ACK/retry/fail, crash recover (T-008, D31–D32).

use rusqlite::{params, OptionalExtension};

use crate::error::CoreError;
use crate::model::{AccountId, OpKind, OpStatus, Operation, OperationId};
use crate::provider::{ApplyError, MailProvider};
use crate::store::{
    apply_undo, archive_move_history, json_string_field, mailbox_remote_id, sql_err, Core,
};

/// D32: 2s, 5s, 15s, 30s, 60s, then exp backoff capped at 15 minutes.
pub fn retry_delay_secs(failures: u32) -> i64 {
    // D32 lives in `feathermail-sync` because the folder scheduler and
    // the connection machine need the same table and cannot reach this
    // crate. Delegating keeps one copy instead of three.
    feathermail_sync::backoff::backoff_delay_secs(failures)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TickOutcome {
    Idle,
    Acked(OperationId),
    Retry { id: OperationId, delay: i64 },
    Failed { id: OperationId, error: ApplyError },
}

/// T-087: how many rows in `operations` are still waiting to reach the
/// server (`pending`) versus have given up for good (`failed`) --
/// [`Core::queue_counts`]'s answer, and what the Diagnostics page's
/// "Pending operations" field reads instead of the hardcoded `0` it used
/// to show.
///
/// Kept as two numbers, not folded into one sum: a mark stuck in `failed`
/// (it will never leave this machine without the user retrying or
/// undoing it) and a mark merely waiting its turn are different facts for
/// someone staring at this page because "my star didn't stick" -- one
/// combined count would hide exactly which one is true, which is the
/// same "plausible number instead of the truth" failure T-087 was filed
/// over for the other three fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueCounts {
    pub pending: u32,
    pub failed: u32,
}

impl Core {
    /// `running` → `pending` so a kill mid-apply is picked up after reopen (D31, §56).
    pub fn recover_inflight(&self) -> Result<u32, CoreError> {
        let n = self
            .db
            .conn()
            .execute(
                "UPDATE operations SET status = 'pending' WHERE status = 'running'",
                [],
            )
            .map_err(sql_err)?;
        Ok(n as u32)
    }

    /// T-087: `pending` and `failed` row counts in `operations`, read
    /// fresh on every call -- a plain `SELECT`/`COUNT`, not a cached
    /// field, so it can never go stale the way a value copied into `App`
    /// at startup would (see the Diagnostics page's doc comment on why it
    /// re-reads this on open rather than caching it once).
    ///
    /// `running` is deliberately not counted in either bucket: an
    /// operation sits at `running` only for the span of one
    /// `apply_claimed` call -- synchronous SQL plus one blocking provider
    /// round trip, no yield point in between -- so on a real machine no
    /// caller of this method can ever observe a `running` row; counting
    /// it would just be a third bucket nothing outside a torn crash ever
    /// populates.
    pub fn queue_counts(&self) -> Result<QueueCounts, CoreError> {
        let conn = self.db.conn();
        let (pending, failed) = conn
            .query_row(
                "SELECT COALESCE(SUM(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0)
                 FROM operations",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(sql_err)?;
        Ok(QueueCounts {
            pending: pending as u32,
            failed: failed as u32,
        })
    }

    /// Claims the oldest due operation across **every** account and applies
    /// it. Correct only when the caller's `provider` can serve any account
    /// -- which a live IMAP session cannot: it is logged in to exactly one
    /// mailbox. A background worker holding one connection must use
    /// [`Core::tick_for_account`] instead, or it will eventually run one
    /// account's `UID MOVE` down another account's socket.
    pub fn tick(&mut self, provider: &mut dyn MailProvider) -> Result<TickOutcome, CoreError> {
        let now = self.now();
        let Some(op) = self.claim_next(now, None)? else {
            return Ok(TickOutcome::Idle);
        };
        self.apply_claimed(op, provider)
    }

    /// [`Core::tick`] restricted to one account (T-078): claims and applies
    /// the oldest operation due **for `account`** and nothing else.
    ///
    /// This is the entry point a live connection needs. An IMAP session is
    /// authenticated to one mailbox, so handing `tick` a provider connected
    /// for account A while the globally-oldest due operation belongs to
    /// account B would send B's `UID MOVE`/`UID STORE` over A's socket --
    /// against whatever UIDs happen to sit at those numbers in A's folder.
    /// Since T-081 that mistake is not even loud: the apply fails, the
    /// failure is non-retryable, and the queue helpfully rolls back the
    /// user's local mark.
    ///
    /// [`TickOutcome::Idle`] here means "nothing due for *this* account",
    /// which is not the same as "nothing due at all" -- a caller that
    /// round-robins over accounts must not read it as a reason to go to
    /// sleep until it has asked every account.
    pub fn tick_for_account(
        &mut self,
        account: &AccountId,
        provider: &mut dyn MailProvider,
    ) -> Result<TickOutcome, CoreError> {
        let now = self.now();
        let Some(op) = self.claim_next(now, Some(account.as_str()))? else {
            return Ok(TickOutcome::Idle);
        };
        self.apply_claimed(op, provider)
    }

    fn apply_claimed(
        &mut self,
        op: Operation,
        provider: &mut dyn MailProvider,
    ) -> Result<TickOutcome, CoreError> {
        let now = self.now();
        match provider.apply(&op) {
            Ok(()) | Err(ApplyError::Conflict) => {
                self.finish(&op.id, OpStatus::Acked, None, None)?;
                Ok(TickOutcome::Acked(op.id))
            }
            Err(err) if err.retry() => {
                let failures = op.retry_count + 1;
                let delay = retry_delay_secs(failures);
                self.finish(&op.id, OpStatus::Pending, Some(failures), Some(now + delay))?;
                Ok(TickOutcome::Retry { id: op.id, delay })
            }
            Err(err) => {
                // T-081: this is a non-retryable, terminal failure --
                // `Network` already branched above (retries), `Conflict`
                // is treated as success (first match arm). `dispatch`
                // wrote the local mark optimistically, before the server
                // ever confirmed anything; now that the provider has said
                // it never will, undo it so SQLite stops asserting a state
                // that doesn't exist on the server.
                self.fail_and_undo(&op, op.retry_count + 1)?;
                if matches!(err, ApplyError::Auth) {
                    self.db
                        .conn()
                        .execute(
                            "UPDATE accounts SET status = 'error', updated_at = ?1 WHERE id = ?2",
                            params![now, op.account_id.as_str()],
                        )
                        .map_err(sql_err)?;
                }
                Ok(TickOutcome::Failed {
                    id: op.id,
                    error: err,
                })
            }
        }
    }

    /// `account = None` claims across every account ([`Core::tick`]);
    /// `Some(id)` restricts to one ([`Core::tick_for_account`]).
    ///
    /// `created_at` is `Core::now()`, i.e. whole seconds, so two commands
    /// the user issued back to back almost always carry the same value and
    /// the tie-break decides the real order. `operations.seq` is that
    /// tie-break (T-162, schema v29): `store::enqueue` hands out
    /// `MAX(seq) + 1` on every INSERT *and* on every revive. It replaced
    /// `id ASC`, which sorted by the operation *kind's* name (`archive` <
    /// `star`) and so applied a Star issued before an Archive after it --
    /// a flag STORE against a UID the MOVE had already taken away, which
    /// the server answers OK and silently does nothing for -- and then
    /// `rowid`, which SQLite fixes at the first INSERT and never moves
    /// again. That was right for a queue that only grows and wrong for the
    /// one this is: `enqueue` revives a `failed`/`acked` row in place
    /// (D29's idempotency key lands a repeated command on the row the
    /// first one wrote), so under `rowid` a Star issued ten minutes ago and
    /// revived now was claimed ahead of the Move issued a second ago.
    fn claim_next(&self, now: i64, account: Option<&str>) -> Result<Option<Operation>, CoreError> {
        let conn = self.db.conn();
        let sql = format!(
            "SELECT id, account_id, target_id, op, payload, payload_hash,
                    created_at, retry_count, next_attempt_at, status, undo_of
             FROM operations
             WHERE status = 'pending'
               AND (next_attempt_at IS NULL OR next_attempt_at <= ?1){}
             ORDER BY created_at ASC, seq ASC
             LIMIT 1",
            if account.is_some() {
                "\n               AND account_id = ?2"
            } else {
                ""
            }
        );
        let op = match account {
            Some(account) => conn
                .query_row(&sql, params![now, account], map_op)
                .optional()
                .map_err(sql_err)?,
            None => conn
                .query_row(&sql, params![now], map_op)
                .optional()
                .map_err(sql_err)?,
        };
        let Some(op) = op else {
            return Ok(None);
        };
        let n = conn
            .execute(
                "UPDATE operations SET status = 'running' WHERE id = ?1 AND status = 'pending'",
                params![op.id.as_str()],
            )
            .map_err(sql_err)?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(op))
    }

    /// T-081: mark `op` `Failed` and, in the same transaction, restore
    /// whatever `threads` columns its `undo_payload` names (see
    /// `crate::store::undo_snapshot`/`apply_undo`) -- one commit, so a
    /// crash between the two can never leave the thread rolled back with
    /// the operation still looking retryable, or vice versa.
    fn fail_and_undo(&self, op: &Operation, retry_count: u32) -> Result<(), CoreError> {
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        let undo: Option<String> = tx
            .query_row(
                "SELECT undo_payload FROM operations WHERE id = ?1",
                params![op.id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        if let Some(undo) = undo {
            if !undo.is_empty() {
                apply_undo(&tx, op.account_id.as_str(), &op.target_id, &undo)?;
            }
        }
        tx.execute(
            "UPDATE operations SET status = 'failed', retry_count = ?1, next_attempt_at = NULL
             WHERE id = ?2",
            params![retry_count as i64, op.id.as_str()],
        )
        .map_err(sql_err)?;
        tx.execute(
            "UPDATE operations SET status = 'cancelled', next_attempt_at = NULL
             WHERE undo_of = ?1 AND status = 'blocked'",
            params![op.id.as_str()],
        )
        .map_err(sql_err)?;
        // A terminal failure invalidates the durable MOVE intent as well as
        // the optimistic thread mark. Destination sync may have run first;
        // rehome that one known local row back to its captured source before
        // deleting the intent, so no failed operation can keep stealing
        // future unrelated destination mail.
        tx.execute(
            "UPDATE messages SET
                folder_id = (SELECT source_folder_id FROM operation_moves
                             WHERE operation_id = ?1 AND message_id = messages.id),
                provider_uid = (SELECT source_uid FROM operation_moves
                                WHERE operation_id = ?1 AND message_id = messages.id)
             WHERE id IN (SELECT message_id FROM operation_moves WHERE operation_id = ?1)",
            params![op.id.as_str()],
        )
        .map_err(sql_err)?;
        archive_move_history(&tx, op.id.as_str())?;
        tx.execute(
            "DELETE FROM operation_moves WHERE operation_id = ?1",
            params![op.id.as_str()],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }

    fn finish(
        &self,
        id: &OperationId,
        status: OpStatus,
        retry_count: Option<u32>,
        next_attempt_at: Option<i64>,
    ) -> Result<(), CoreError> {
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        match (retry_count, next_attempt_at) {
            (Some(n), Some(at)) => tx
                .execute(
                    "UPDATE operations SET status = ?1, retry_count = ?2, next_attempt_at = ?3
                     WHERE id = ?4",
                    params![status.as_str(), n as i64, at, id.as_str()],
                )
                .map_err(sql_err)?,
            (Some(n), None) => tx
                .execute(
                    "UPDATE operations SET status = ?1, retry_count = ?2, next_attempt_at = NULL
                     WHERE id = ?3",
                    params![status.as_str(), n as i64, id.as_str()],
                )
                .map_err(sql_err)?,
            (None, _) => tx
                .execute(
                    "UPDATE operations SET status = ?1, next_attempt_at = NULL WHERE id = ?2",
                    params![status.as_str(), id.as_str()],
                )
                .map_err(sql_err)?,
        };
        if status == OpStatus::Acked {
            // Only a wire ACK makes the optimistic folder change eligible
            // for reconciliation.  Dispatch deliberately leaves existing
            // scheduler clocks untouched: before this point a lost response
            // must still be retried, and an eager destination sync must not
            // mistake an empty mailbox for a completed move.
            tx.execute(
                "UPDATE sync_state SET last_sync_at = NULL,
                    last_attempt_at = NULL, consecutive_failures = 0
                 WHERE account_id = (SELECT account_id FROM operations WHERE id = ?1)
                   AND folder_id IN (
                       SELECT source_folder_id FROM operation_moves WHERE operation_id = ?1
                       UNION
                       SELECT destination_folder_id FROM operation_moves WHERE operation_id = ?1
                   )",
                params![id.as_str()],
            )
            .map_err(sql_err)?;
            // Destination sync may already have supplied the new UID. In
            // that case ACK can safely move the stable row to the real
            // destination immediately; when it is still NULL, source sync
            // remains the locator until destination observation completes.
            tx.execute(
                "UPDATE messages SET folder_id = (
                        SELECT destination_folder_id FROM operation_moves
                        WHERE operation_id = ?1 AND message_id = messages.id
                          AND destination_uid IS NOT NULL
                    ), provider_uid = (
                        SELECT destination_uid FROM operation_moves
                        WHERE operation_id = ?1 AND message_id = messages.id
                          AND destination_uid IS NOT NULL
                    )
                 WHERE id IN (
                    SELECT message_id FROM operation_moves
                    WHERE operation_id = ?1 AND destination_uid IS NOT NULL
                 )",
                params![id.as_str()],
            )
            .map_err(sql_err)?;
            // If destination sync happened before this ACK, the stable row
            // is already in the destination and source observation cannot
            // add any further information.  Once both facts are true, drop
            // the durable intent now; otherwise retain it until source sync
            // proves the old UID has vanished.
            tx.execute(
                "DELETE FROM operation_moves
                 WHERE operation_id = ?1 AND destination_uid IS NOT NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM messages m
                       WHERE m.id = operation_moves.message_id
                         AND m.folder_id = operation_moves.source_folder_id
                         AND m.provider_uid = operation_moves.source_uid
                   )",
                params![id.as_str()],
            )
            .map_err(sql_err)?;
            // Keep the coordinates after the active intent is gone so a
            // later Undo can materialize the inverse from durable history.
            archive_move_history(&tx, id.as_str())?;
            settle_folder_rename(&tx, id.as_str())?;
            settle_folder_delete(&tx, id.as_str())?;
        }
        // A reverse created while this operation was running is released
        // only after the predecessor is confirmed. For moves, the helper
        // also waits until destination sync has supplied every destination
        // UID; without that locator, claiming the reverse would guess.
        release_blocked_undo(&tx, id.as_str())?;
        tx.commit().map_err(sql_err)?;
        Ok(())
    }
}

/// T-060t: a `RenameFolder` ACK is the one moment the folder's *identity*
/// moves, so it happens inside the same transaction as the ACK itself.
///
/// A crash between "operation acked" and "remote_id updated" would leave
/// the local row pointing at a mailbox that no longer exists; the next
/// `LIST` would then see the new name as an unknown mailbox and open a
/// second row beside the first, stranding the local mail under a folder
/// nothing syncs. One commit removes that window entirely.
///
/// The `WHERE remote_id = from` guard makes this idempotent and safe
/// against a stale payload: if identity already moved (a server-side rename
/// discovered first, or a replayed ACK), nothing matches and nothing is
/// overwritten.
///
/// T-158: the destination is rebuilt from the payload's parts through
/// [`mailbox_remote_id`] -- the same function `ImapMailProvider` builds the
/// `RENAME` argument with, so the id written here is by construction the
/// mailbox the server was actually asked for. A payload queued before that
/// change carries a ready-made `to` instead; it is applied as it always
/// was, since the operation it belongs to is being sent the old way too.
fn settle_folder_rename(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<(), CoreError> {
    let row: Option<(String, String, String)> = tx
        .query_row(
            "SELECT account_id, target_id, payload FROM operations
             WHERE id = ?1 AND op = 'rename_folder'",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((account_id, folder_id, payload)) = row else {
        return Ok(());
    };
    let Some(from) = json_string_field(&payload, "from") else {
        return Ok(());
    };
    let Some(to) = rename_destination(&payload) else {
        return Ok(());
    };
    tx.execute(
        "UPDATE folders SET remote_id = ?1
         WHERE account_id = ?2 AND id = ?3 AND remote_id = ?4",
        params![to, account_id, folder_id, from],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// The mailbox path a `rename_folder` payload asks for, on the wire.
///
/// Current payloads carry `parent_remote_id` + `delimiter` + the raw
/// `label`; one queued before T-158 carries a pre-joined `to`. Kept in
/// this order deliberately: a payload that has `label` is a new one and
/// must be built the new way even if some future field were named `to`.
pub(crate) fn rename_destination(payload: &str) -> Option<String> {
    if let Some(label) = json_string_field(payload, "label") {
        let parent = json_string_field(payload, "parent_remote_id").unwrap_or_default();
        let delimiter = json_string_field(payload, "delimiter").unwrap_or_default();
        return Some(mailbox_remote_id(&parent, &delimiter, &label));
    }
    json_string_field(payload, "to")
}

/// T-060u: a `DeleteFolder` ACK is the moment the mailbox stops existing, so
/// the local row stops owning it in the same transaction.
///
/// Until then the tombstone keeps `remote_id`, which is what makes the whole
/// design safe: a `LIST` walk that runs while the `DELETE` is still queued
/// matches this row by identity instead of opening a second one, and a
/// terminal failure leaves the folder discoverable exactly where it was. Once
/// the server confirms, keeping the id would be a lie -- and would let a
/// later walk resurrect a folder the user deleted, or block a new mailbox of
/// the same name through `UNIQUE (account_id, remote_id)`.
///
/// The `WHERE remote_id = mailbox` guard makes it idempotent and refuses to
/// act on a stale payload, exactly as in [`settle_folder_rename`].
fn settle_folder_delete(tx: &rusqlite::Transaction<'_>, id: &str) -> Result<(), CoreError> {
    let row: Option<(String, String, String)> = tx
        .query_row(
            "SELECT account_id, target_id, payload FROM operations
             WHERE id = ?1 AND op = 'delete_folder'",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((account_id, folder_id, payload)) = row else {
        return Ok(());
    };
    let Some(mailbox) = json_string_field(&payload, "mailbox") else {
        return Ok(());
    };
    tx.execute(
        "UPDATE folders SET remote_id = NULL
         WHERE account_id = ?1 AND id = ?2 AND remote_id = ?3",
        params![account_id, folder_id, mailbox],
    )
    .map_err(sql_err)?;
    Ok(())
}

pub(crate) fn release_blocked_undo(
    tx: &rusqlite::Transaction<'_>,
    original_id: &str,
) -> Result<(), CoreError> {
    tx.execute(
        "UPDATE operations SET status = 'pending', next_attempt_at = NULL
         WHERE undo_of = ?1 AND status = 'blocked'
           AND NOT EXISTS (
               SELECT 1 FROM operation_moves om
               WHERE om.operation_id = ?1 AND om.destination_uid IS NULL
           )
           AND NOT EXISTS (
               SELECT 1 FROM operation_move_history h
               WHERE h.operation_id = ?1 AND h.destination_uid IS NULL
                 AND NOT EXISTS (
                     SELECT 1 FROM operation_moves active
                     WHERE active.operation_id = h.operation_id
                       AND active.message_id = h.message_id
                 )
           )",
        params![original_id],
    )
    .map_err(sql_err)?;
    Ok(())
}

fn map_op(row: &rusqlite::Row<'_>) -> rusqlite::Result<Operation> {
    let kind: String = row.get(3)?;
    let kind: OpKind = kind.parse().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown op kind",
            )),
        )
    })?;
    let status: String = row.get(9)?;
    let status: OpStatus = status.parse().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            9,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unknown op status",
            )),
        )
    })?;
    Ok(Operation {
        id: OperationId(row.get(0)?),
        account_id: AccountId(row.get(1)?),
        target_id: row.get(2)?,
        kind,
        payload: row.get(4)?,
        payload_hash: row.get(5)?,
        created_at: row.get(6)?,
        retry_count: row.get::<_, i64>(7)? as u32,
        next_attempt_at: row.get(8)?,
        status,
        undo_of: row.get::<_, Option<String>>(10)?.map(OperationId),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;
    use crate::error::ErrorCode;
    use crate::model::{FolderId, ThreadId, FIXTURE_NOW};
    use crate::provider::{Reauthenticate, ReauthingProvider};
    use crate::store::Core;
    use rusqlite::params;

    struct FakeProvider {
        fail_network: u32,
        fail_auth: bool,
        applies: Vec<(OpKind, String)>,
    }

    impl FakeProvider {
        fn ok() -> Self {
            Self {
                fail_network: 0,
                fail_auth: false,
                applies: Vec::new(),
            }
        }
    }

    impl MailProvider for FakeProvider {
        fn apply(&mut self, op: &Operation) -> Result<(), ApplyError> {
            if self.fail_auth {
                return Err(ApplyError::Auth);
            }
            if self.fail_network > 0 {
                self.fail_network -= 1;
                return Err(ApplyError::Network);
            }
            self.applies.push((op.kind, op.target_id.clone()));
            Ok(())
        }
    }

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
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
             VALUES ('t1', 'john', 'inbox', 'Hello', 'Hi', ?1, 1)",
            params![FIXTURE_NOW],
        )
        .unwrap();
    }

    /// A second account with its own Inbox and one thread, for the T-078
    /// account-scoping tests.
    fn seed_second_account(core: &Core, account: &str) {
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, 'Jane Roe', 'jane@example.com', 'generic', 'synced', 'recent', 0, 0)",
            params![account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, name, kind) VALUES ('inbox2', ?1, 'Inbox', 'inbox')",
            params![account],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
             VALUES ('t2', ?1, 'inbox2', 'Hello', 'Hi', ?2, 1)",
            params![account, FIXTURE_NOW],
        )
        .unwrap();
    }

    fn archive(core: &mut Core) {
        core.dispatch(Command::Archive {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
    }

    fn op_status(core: &Core) -> (String, i64, Option<i64>) {
        core.db
            .conn()
            .query_row(
                "SELECT status, retry_count, next_attempt_at FROM operations WHERE target_id = 't1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    fn archived(core: &Core) -> bool {
        core.get_thread(&AccountId("john".into()), &ThreadId("t1".into()))
            .unwrap()
            .archived()
    }

    #[test]
    fn retry_table_matches_d32() {
        assert_eq!(retry_delay_secs(0), 0);
        assert_eq!(retry_delay_secs(1), 2);
        assert_eq!(retry_delay_secs(2), 5);
        assert_eq!(retry_delay_secs(3), 15);
        assert_eq!(retry_delay_secs(4), 30);
        assert_eq!(retry_delay_secs(5), 60);
        assert_eq!(retry_delay_secs(6), 120);
        assert_eq!(retry_delay_secs(7), 240);
        assert_eq!(retry_delay_secs(8), 480);
        assert_eq!(retry_delay_secs(9), 900);
        assert_eq!(retry_delay_secs(20), 900);
    }

    #[test]
    fn three_network_failures_use_retry_delays() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        archive(&mut core);
        let mut provider = FakeProvider {
            fail_network: 3,
            fail_auth: false,
            applies: Vec::new(),
        };
        let mut now = FIXTURE_NOW;
        let mut delays = Vec::new();
        for _ in 0..3 {
            core.set_now(now);
            match core.tick(&mut provider).unwrap() {
                TickOutcome::Retry { delay, .. } => {
                    delays.push(delay);
                    now += delay;
                }
                other => panic!("expected retry, got {other:?}"),
            }
            assert_eq!(
                core.tick(&mut provider).unwrap(),
                TickOutcome::Idle,
                "must not tight-loop before next_attempt_at"
            );
        }
        assert_eq!(delays, vec![2, 5, 15]);
        core.set_now(now);
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Acked(_)
        ));
        assert_eq!(provider.applies.len(), 1);
        assert_eq!(op_status(&core).0, "acked");
        assert!(archived(&core));
    }

    #[test]
    fn kill_mid_op_recovers_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let mut core = Core::open(&path).unwrap();
            core.set_now(FIXTURE_NOW);
            seed(&core);
            archive(&mut core);
            core.db
                .conn()
                .execute(
                    "UPDATE operations SET status = 'running' WHERE target_id = 't1'",
                    [],
                )
                .unwrap();
            assert_eq!(op_status(&core).0, "running");
        }
        let mut core = Core::open(&path).unwrap();
        core.set_now(FIXTURE_NOW);
        assert_eq!(op_status(&core).0, "pending");
        let mut provider = FakeProvider::ok();
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Acked(_)
        ));
        assert_eq!(provider.applies.len(), 1);
        assert!(archived(&core));
        let n: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn idempotent_replay_does_not_duplicate_flag() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        archive(&mut core);
        let mut provider = FakeProvider::ok();
        core.tick(&mut provider).unwrap();
        assert!(archived(&core));
        core.db
            .conn()
            .execute(
                "UPDATE operations SET status = 'pending', next_attempt_at = NULL WHERE target_id = 't1'",
                [],
            )
            .unwrap();
        core.tick(&mut provider).unwrap();
        assert!(archived(&core));
        assert_eq!(provider.applies.len(), 2);
        let (archived_flag, n): (i64, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT archived, (SELECT COUNT(*) FROM operations) FROM threads WHERE id = 't1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(archived_flag, 1);
        assert_eq!(n, 1);
    }

    #[test]
    fn auth_fail_does_not_retry() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        archive(&mut core);
        let mut provider = FakeProvider {
            fail_network: 0,
            fail_auth: true,
            applies: Vec::new(),
        };
        match core.tick(&mut provider).unwrap() {
            TickOutcome::Failed { error, .. } => {
                assert_eq!(error, ApplyError::Auth);
                assert_eq!(error.code(), ErrorCode::AuthRequired);
            }
            other => panic!("expected failed, got {other:?}"),
        }
        assert_eq!(op_status(&core).0, "failed");
        let status: String = core
            .db
            .conn()
            .query_row("SELECT status FROM accounts WHERE id = 'john'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "error");
        core.set_now(FIXTURE_NOW + 10_000);
        assert_eq!(core.tick(&mut provider).unwrap(), TickOutcome::Idle);
        assert!(provider.applies.is_empty());
    }

    // --- T-083: an *expired* access token (a `refresh_token` exists and
    // can fix it) must not be treated the same as a *revoked* one. Bare
    // `Core::tick` above (`auth_fail_does_not_retry`) still treats every
    // `Auth` as terminal -- and must keep doing so for a caller that
    // hands it a raw provider. What changes is that a caller can now wrap
    // that provider in `ReauthingProvider` (`crates/core/src/provider.rs`)
    // so an `Auth` that a `Reauthenticate` impl can actually fix never
    // reaches `Core::tick` as `Auth` at all -- these tests exercise that
    // through the real `Core::tick` queue-draining path, not just the
    // wrapper in isolation (see `provider::reauth_tests` for that).

    fn seed_thread(core: &Core, id: &str) {
        core.db
            .conn()
            .execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
                 VALUES (?1, 'john', 'inbox', 'Hello', 'Hi', ?2, 1)",
                params![id, FIXTURE_NOW],
            )
            .unwrap();
    }

    fn is_archived(core: &Core, id: &str) -> bool {
        core.db
            .conn()
            .query_row(
                "SELECT archived FROM threads WHERE id = ?1",
                params![id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    }

    fn account_status(core: &Core) -> String {
        core.db
            .conn()
            .query_row("SELECT status FROM accounts WHERE id = 'john'", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    /// Fails every `apply()` with `Auth` until `fixed`, then always
    /// succeeds -- models "every queued operation hits the same expired
    /// token" (the bug T-083 fixes), one level up from the unit fakes in
    /// `provider::reauth_tests`: this one is driven through the real
    /// `Core::tick` queue, not called directly.
    struct ExpiringTokenProvider {
        fixed: bool,
        acked: std::rc::Rc<std::cell::Cell<u32>>,
    }

    impl MailProvider for ExpiringTokenProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            if !self.fixed {
                return Err(ApplyError::Auth);
            }
            self.acked.set(self.acked.get() + 1);
            Ok(())
        }
    }

    /// `ReauthingProvider`'s own fields are private to `provider.rs` (not
    /// reachable from this sibling module), so calls and outcomes here are
    /// observed through shared cells handed to the fake up front instead
    /// of read back off the wrapper afterwards.
    struct CountingReauth {
        calls: std::rc::Rc<std::cell::Cell<u32>>,
        ok: bool,
        acked: std::rc::Rc<std::cell::Cell<u32>>,
    }

    impl Reauthenticate<ExpiringTokenProvider> for CountingReauth {
        fn reauthenticate(&mut self) -> Result<ExpiringTokenProvider, ApplyError> {
            self.calls.set(self.calls.get() + 1);
            if self.ok {
                Ok(ExpiringTokenProvider {
                    fixed: true,
                    acked: self.acked.clone(),
                })
            } else {
                Err(ApplyError::Auth)
            }
        }
    }

    #[test]
    fn expired_token_recovers_the_whole_queue_without_rolling_back_any_mark() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_thread(&core, "t2");
        seed_thread(&core, "t3");
        core.dispatch(Command::Archive {
            account_id: AccountId("john".into()),
            thread_ids: vec![
                ThreadId("t1".into()),
                ThreadId("t2".into()),
                ThreadId("t3".into()),
            ],
        })
        .unwrap();
        assert_eq!(op_count(&core), 3);

        let acked = std::rc::Rc::new(std::cell::Cell::new(0));
        let reauth_calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut provider = ReauthingProvider::new(
            ExpiringTokenProvider {
                fixed: false,
                acked: acked.clone(),
            },
            CountingReauth {
                calls: reauth_calls.clone(),
                ok: true,
                acked: acked.clone(),
            },
        );

        let mut outcomes = Vec::new();
        loop {
            match core.tick(&mut provider).unwrap() {
                TickOutcome::Idle => break,
                other => outcomes.push(other),
            }
        }

        assert_eq!(
            outcomes.len(),
            3,
            "all three queued ops must reach the server: {outcomes:?}"
        );
        assert!(
            outcomes.iter().all(|o| matches!(o, TickOutcome::Acked(_))),
            "every op must ack, none may fail: {outcomes:?}"
        );
        assert_eq!(acked.get(), 3);
        // The whole point: one refresh fixes the session for the rest of
        // the queue -- not one refresh per operation.
        assert_eq!(
            reauth_calls.get(),
            1,
            "only the first op's Auth should trigger a refresh"
        );

        for id in ["t1", "t2", "t3"] {
            assert!(
                is_archived(&core, id),
                "{id} must stay archived -- a recoverable Auth must never roll back a mark"
            );
        }
        assert_eq!(
            account_status(&core),
            "synced",
            "a merely-expired token must not flip the account into 'error'"
        );
    }

    #[test]
    fn genuinely_revoked_auth_still_rolls_back_and_reports_through_reauthing_provider() {
        // The flip side: wrapping the provider must not turn a real,
        // unfixable auth failure into a silent success. If the refresh
        // itself fails (revoked grant, no refresh_token, whatever),
        // `Auth` must still reach `Core::tick` and behave exactly like
        // `auth_fail_does_not_retry` above -- rollback and `status =
        // 'error'`, not swallowed by the wrapper.
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        archive(&mut core);

        let acked = std::rc::Rc::new(std::cell::Cell::new(0));
        let reauth_calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let mut provider = ReauthingProvider::new(
            ExpiringTokenProvider {
                fixed: false,
                acked: acked.clone(),
            },
            CountingReauth {
                calls: reauth_calls.clone(),
                ok: false,
                acked,
            },
        );

        match core.tick(&mut provider).unwrap() {
            TickOutcome::Failed { error, .. } => assert_eq!(error, ApplyError::Auth),
            other => panic!("expected Failed(Auth), got {other:?}"),
        }
        assert_eq!(reauth_calls.get(), 1);
        assert_eq!(op_status(&core).0, "failed");
        assert!(
            !archived(&core),
            "a terminal Auth must still roll back the optimistic mark"
        );
        assert_eq!(account_status(&core), "error");
    }

    // --- T-081: a non-retryable failure must roll back the optimistic
    // local mark `Core::dispatch` wrote, instead of leaving it standing
    // forever with no operation left to reconcile it against the server.

    /// Always returns the same error, whatever `op` it's handed.
    struct AlwaysFailProvider(ApplyError);

    impl MailProvider for AlwaysFailProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Err(self.0)
        }
    }

    /// Fails with `error` on its first call, then succeeds on every call
    /// after that -- for T-081's "the retried command really reaches the
    /// provider" acceptance test.
    struct FailOnceThenOk {
        error: ApplyError,
        failed_once: bool,
        applies: Vec<(OpKind, String)>,
    }

    impl FailOnceThenOk {
        fn new(error: ApplyError) -> Self {
            Self {
                error,
                failed_once: false,
                applies: Vec::new(),
            }
        }
    }

    impl MailProvider for FailOnceThenOk {
        fn apply(&mut self, op: &Operation) -> Result<(), ApplyError> {
            if !self.failed_once {
                self.failed_once = true;
                return Err(self.error);
            }
            self.applies.push((op.kind, op.target_id.clone()));
            Ok(())
        }
    }

    type RawThread = (i64, i64, i64, i64, Option<i64>, String);

    /// The raw `threads` row for `id`: (unread, starred, archived, deleted,
    /// snooze_until, folder_id) -- read directly rather than through
    /// [`Core::get_thread`]'s [`crate::model::Placement`] mapping, so a
    /// test can assert the exact pre-command row came back, not just that
    /// it maps to the same `Placement`.
    fn raw_thread(core: &Core, id: &str) -> RawThread {
        core.db
            .conn()
            .query_row(
                "SELECT unread, starred, archived, deleted, snooze_until, folder_id
                 FROM threads WHERE id = ?1",
                params![id],
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
            .unwrap()
    }

    fn op_status_and_retries(core: &Core) -> (String, i64) {
        core.db
            .conn()
            .query_row(
                "SELECT status, retry_count FROM operations WHERE target_id = 't1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    fn op_count(core: &Core) -> i64 {
        core.db
            .conn()
            .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn unsupported_archive_rolls_back_the_local_mark() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let before = raw_thread(&core, "t1");
        archive(&mut core);
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Failed {
                error: ApplyError::Unsupported,
                ..
            }
        ));
        assert_eq!(op_status(&core).0, "failed");
        assert_eq!(
            raw_thread(&core, "t1"),
            before,
            "failed Archive must restore the thread to its pre-command row"
        );
    }

    #[test]
    fn unsupported_trash_rolls_back_the_local_mark() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let before = raw_thread(&core, "t1");
        core.dispatch(Command::Trash {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();
        assert_eq!(op_status(&core).0, "failed");
        assert_eq!(raw_thread(&core, "t1"), before);
    }

    #[test]
    fn failed_real_trash_restores_archived_flag_and_folder() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
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
                "UPDATE threads SET archived = 1, unread = 1 WHERE id = 't1'",
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
        let before = raw_thread(&core, "t1");
        core.dispatch(Command::Trash {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();
        assert_eq!(op_status(&core).0, "failed");
        assert_eq!(raw_thread(&core, "t1"), before);
    }

    #[test]
    fn real_move_keeps_sync_clocks_until_ack_then_marks_both_folders_due() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
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
                 VALUES ('archive', 'john', 'Archive', 'Archive', 'archive')",
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
        for folder in ["inbox", "archive"] {
            core.db
                .conn()
                .execute(
                    "INSERT INTO sync_state
                        (account_id, folder_id, last_sync_at, last_attempt_at, consecutive_failures)
                     VALUES ('john', ?1, 100, 110, 2)",
                    params![folder],
                )
                .unwrap();
        }
        core.dispatch(Command::Archive {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let clocks_after_dispatch: (Option<i64>, Option<i64>, Option<i64>, Option<i64>) = core
            .db
            .conn()
            .query_row(
                "SELECT
                    (SELECT last_sync_at FROM sync_state WHERE folder_id = 'inbox'),
                    (SELECT last_attempt_at FROM sync_state WHERE folder_id = 'inbox'),
                    (SELECT last_sync_at FROM sync_state WHERE folder_id = 'archive'),
                    (SELECT last_attempt_at FROM sync_state WHERE folder_id = 'archive')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            clocks_after_dispatch,
            (Some(100), Some(110), Some(100), Some(110))
        );

        let mut provider = FakeProvider::ok();
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Acked(_)
        ));
        let clocks_after_ack: (Option<i64>, Option<i64>, i64, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT
                    (SELECT last_sync_at FROM sync_state WHERE folder_id = 'inbox'),
                    (SELECT last_sync_at FROM sync_state WHERE folder_id = 'archive'),
                    (SELECT consecutive_failures FROM sync_state WHERE folder_id = 'inbox'),
                    (SELECT consecutive_failures FROM sync_state WHERE folder_id = 'archive')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(clocks_after_ack, (None, None, 0, 0));
    }

    #[test]
    fn unsupported_permanent_delete_rolls_back_the_local_mark() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let before = raw_thread(&core, "t1");
        core.dispatch(Command::PermanentDelete {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();
        assert_eq!(op_status(&core).0, "failed");
        assert_eq!(raw_thread(&core, "t1"), before);
    }

    #[test]
    fn unsupported_mark_read_rolls_back_the_local_mark() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core); // t1 starts unread = 1
        let before = raw_thread(&core, "t1");
        core.dispatch(Command::MarkRead {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();
        assert_eq!(op_status(&core).0, "failed");
        assert_eq!(raw_thread(&core, "t1"), before);
    }

    #[test]
    fn unsupported_mark_unread_rolls_back_the_local_mark() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        core.db
            .conn()
            .execute("UPDATE threads SET unread = 0 WHERE id = 't1'", [])
            .unwrap();
        let before = raw_thread(&core, "t1");
        core.dispatch(Command::MarkUnread {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();
        assert_eq!(op_status(&core).0, "failed");
        assert_eq!(raw_thread(&core, "t1"), before);
    }

    #[test]
    fn unsupported_star_rolls_back_the_local_mark() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let before = raw_thread(&core, "t1");
        core.dispatch(Command::Star {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();
        assert_eq!(op_status(&core).0, "failed");
        assert_eq!(raw_thread(&core, "t1"), before);
    }

    #[test]
    fn unsupported_unstar_rolls_back_the_local_mark() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        core.db
            .conn()
            .execute("UPDATE threads SET starred = 1 WHERE id = 't1'", [])
            .unwrap();
        let before = raw_thread(&core, "t1");
        core.dispatch(Command::Unstar {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();
        assert_eq!(op_status(&core).0, "failed");
        assert_eq!(raw_thread(&core, "t1"), before);
    }

    #[test]
    fn local_snooze_never_enters_provider_queue() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let before = raw_thread(&core, "t1");
        core.dispatch(Command::Snooze {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
            until: FIXTURE_NOW + 3600,
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Idle
        ));
        let after = raw_thread(&core, "t1");
        assert_ne!(after, before, "local Snooze must remain applied");
        assert_eq!(after.4, Some(FIXTURE_NOW + 3600));
        assert_eq!(op_count(&core), 1, "the local Undo ledger is durable");
        assert_eq!(op_status(&core).0, "local");
        assert_eq!(core.queue_counts().unwrap(), QueueCounts::default());
    }

    /// T-078: a worker holds one live IMAP session, authenticated to one
    /// mailbox. If it could be handed an operation belonging to a
    /// different account, it would run that account's `UID MOVE` against
    /// whatever UIDs sit at those numbers in the mailbox it is actually
    /// connected to.
    #[test]
    fn tick_for_account_never_hands_over_another_accounts_operation() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_second_account(&core, "jane");
        // Jane's is the *older* of the two, so a query that does not
        // filter by account would claim hers first.
        core.dispatch(Command::Archive {
            account_id: AccountId("jane".into()),
            thread_ids: vec![ThreadId("t2".into())],
        })
        .unwrap();
        core.set_now(FIXTURE_NOW + 10);
        core.dispatch(Command::Archive {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();

        let mut provider = FakeProvider::ok();
        let outcome = core
            .tick_for_account(&AccountId("john".into()), &mut provider)
            .unwrap();

        assert!(matches!(outcome, TickOutcome::Acked(_)));
        assert_eq!(
            provider
                .applies
                .iter()
                .map(|(_, target)| target.as_str())
                .collect::<Vec<_>>(),
            vec!["t1"],
            "john's connection must only ever be shown john's operations"
        );
    }

    /// The other half of the same guarantee: `Idle` from
    /// `tick_for_account` means "nothing for this account", and the work
    /// waiting for a different account is still there afterwards.
    #[test]
    fn tick_for_account_is_idle_when_only_another_account_has_work() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_second_account(&core, "jane");
        core.dispatch(Command::Archive {
            account_id: AccountId("jane".into()),
            thread_ids: vec![ThreadId("t2".into())],
        })
        .unwrap();

        let mut provider = FakeProvider::ok();

        assert_eq!(
            core.tick_for_account(&AccountId("john".into()), &mut provider)
                .unwrap(),
            TickOutcome::Idle
        );
        assert!(provider.applies.is_empty());
        // Still claimable by the account it actually belongs to.
        assert!(matches!(
            core.tick_for_account(&AccountId("jane".into()), &mut provider)
                .unwrap(),
            TickOutcome::Acked(_)
        ));
    }

    /// The rollback restores *only* the columns this command's own UPDATE
    /// was about to overwrite. A key absent from `undo_payload` means "this
    /// operation never touched that column", and the rollback has to leave
    /// it exactly as it found it -- not put a default there.
    ///
    /// The case that makes this concrete: a thread is snoozed (that
    /// operation succeeded and the server knows about it), then marked
    /// read, and the `MarkRead` fails non-retryably. `MarkRead`'s snapshot
    /// carries `unread` and nothing else. A reader that treated the
    /// missing `snooze_until` as `null` would silently drop a snooze the
    /// server still holds -- swapping the divergence this task removes for
    /// a different one, in the opposite direction.
    #[test]
    fn rollback_leaves_alone_a_column_the_failed_command_never_touched() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        core.db
            .conn()
            .execute(
                "UPDATE threads SET snooze_until = ?1, unread = 1, starred = 1 WHERE id = 't1'",
                params![FIXTURE_NOW + 3600],
            )
            .unwrap();
        let before = raw_thread(&core, "t1");

        core.dispatch(Command::MarkRead {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();

        assert_eq!(op_status(&core).0, "failed");
        assert_eq!(
            raw_thread(&core, "t1"),
            before,
            "a failed MarkRead must put `unread` back and touch nothing else -- \
             snooze_until and starred were never in its undo snapshot"
        );
    }

    #[test]
    fn unsupported_move_rolls_back_the_local_mark() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, name, kind) VALUES ('work', 'john', 'Work', 'custom')",
                [],
            )
            .unwrap();
        let before = raw_thread(&core, "t1");
        core.dispatch(Command::Move {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
            folder_id: FolderId("work".into()),
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();
        assert_eq!(op_status(&core).0, "failed");
        assert_eq!(
            raw_thread(&core, "t1"),
            before,
            "failed Move must restore the original folder_id, not just the placement flags"
        );
    }

    /// T-081's second acceptance test: the undo must not be guessed from
    /// the command alone. A thread that was *already read* before
    /// `MarkRead` must stay read after `MarkRead` fails -- a naive
    /// "undo of MarkRead is always unread = 1" implementation would flip
    /// it to unread here, which this test catches.
    #[test]
    fn failed_mark_read_on_an_already_read_thread_does_not_flip_it_to_unread() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        core.db
            .conn()
            .execute("UPDATE threads SET unread = 0 WHERE id = 't1'", [])
            .unwrap();
        core.dispatch(Command::MarkRead {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();
        assert_eq!(op_status(&core).0, "failed");
        let (unread, ..) = raw_thread(&core, "t1");
        assert_eq!(
            unread, 0,
            "a thread that was already read must stay read, not become unread"
        );
    }

    /// T-029: `apply_one` now writes `messages.unread`, so a failed apply
    /// must restore members too — otherwise the next rollup would clobber
    /// the threads-level undo. Payload stays thread-level (T-081).
    #[test]
    fn failed_mark_read_restores_messages_unread_not_only_threads() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        core.db
            .conn()
            .execute(
                "INSERT INTO messages (
                    id, account_id, thread_id, folder_id, date,
                    sender_name, sender_email, subject, snippet,
                    unread, starred, has_attachment, importance, size_bytes
                 ) VALUES (
                    'm1', 'john', 't1', 'inbox', ?1,
                    '', 'a@b.com', 'Hello', '',
                    1, 0, 0, 0, 0
                 )",
                params![FIXTURE_NOW],
            )
            .unwrap();
        core.dispatch(Command::MarkRead {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let after_dispatch: i64 = core
            .db
            .conn()
            .query_row("SELECT unread FROM messages WHERE id = 'm1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            after_dispatch, 0,
            "apply_one must write messages.unread, not only threads"
        );
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut provider).unwrap();
        assert_eq!(op_status(&core).0, "failed");
        let after_undo: i64 = core
            .db
            .conn()
            .query_row("SELECT unread FROM messages WHERE id = 'm1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            after_undo, 1,
            "apply_undo must restore messages.unread from the thread-level snapshot"
        );
        let (unread, ..) = raw_thread(&core, "t1");
        assert_eq!(unread, 1);
    }

    /// MarkRead when `threads.unread` is already 0 still clears stale
    /// `messages.unread`. `apply_member_flags` runs even if the thread
    /// UPDATE is a no-op, so a 1:1 profile that T-028 already marked read
    /// cannot keep IMAP's unread bit for the next rollup.
    #[test]
    fn mark_read_on_already_read_thread_still_clears_stale_message_unread() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        core.db
            .conn()
            .execute("UPDATE threads SET unread = 0 WHERE id = 't1'", [])
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO messages (
                    id, account_id, thread_id, folder_id, date,
                    sender_name, sender_email, subject, snippet,
                    unread, starred, has_attachment, importance, size_bytes
                 ) VALUES (
                    'm1', 'john', 't1', 'inbox', ?1,
                    '', 'a@b.com', 'Hello', '',
                    1, 0, 0, 0, 0
                 )",
                params![FIXTURE_NOW],
            )
            .unwrap();
        core.dispatch(Command::MarkRead {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
        })
        .unwrap();
        let unread: i64 = core
            .db
            .conn()
            .query_row("SELECT unread FROM messages WHERE id = 'm1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(unread, 0);
    }

    /// T-081's third acceptance test: `Network` is a retry signal, not a
    /// terminal failure -- the mark must survive untouched while a retry
    /// is still pending.
    #[test]
    fn network_failure_does_not_roll_back_the_local_mark() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        archive(&mut core);
        assert!(archived(&core));
        let mut provider = FakeProvider {
            fail_network: 1,
            fail_auth: false,
            applies: Vec::new(),
        };
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Retry { .. }
        ));
        assert_eq!(
            op_status(&core).0,
            "pending",
            "a retry must stay pending, not failed"
        );
        assert!(
            archived(&core),
            "a Network failure must not roll back the mark -- it is only a retry"
        );
    }

    /// T-081's fourth acceptance test: after a non-retryable failure rolls
    /// the mark back, repeating the same command must really queue a new
    /// attempt -- reviving the `failed` row, not silently dropping it
    /// behind `INSERT OR IGNORE`'s dedup.
    #[test]
    fn repeating_a_failed_command_requeues_and_reaches_the_provider() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        archive(&mut core);
        assert_eq!(op_count(&core), 1);

        let mut failing = AlwaysFailProvider(ApplyError::Unsupported);
        core.tick(&mut failing).unwrap();
        assert_eq!(op_status_and_retries(&core), ("failed".to_string(), 1));
        assert!(
            !archived(&core),
            "the failed attempt must have rolled the mark back"
        );

        // The user tries again: same command, same thread.
        archive(&mut core);
        assert_eq!(
            op_count(&core),
            1,
            "the retry must reuse the same operation id (D29), not add a second row"
        );
        assert_eq!(
            op_status_and_retries(&core),
            ("pending".to_string(), 0),
            "requeueing a failed op must revive it: pending again, retry_count reset"
        );
        assert!(
            archived(&core),
            "dispatch re-applied the local mark for the new attempt"
        );

        let mut succeeding = FailOnceThenOk::new(ApplyError::Unsupported);
        // Consume `FailOnceThenOk`'s single scripted failure on an
        // unrelated call so the tick below actually reaches the provider
        // and succeeds -- proving the requeued op is really in the queue,
        // not just marked pending without being picked up.
        let _ = succeeding.apply(&Operation {
            id: OperationId("throwaway".into()),
            account_id: AccountId("john".into()),
            target_id: "throwaway".into(),
            kind: OpKind::Archive,
            payload: "{}".into(),
            payload_hash: "throwaway".into(),
            created_at: FIXTURE_NOW,
            retry_count: 0,
            next_attempt_at: None,
            status: OpStatus::Pending,
            undo_of: None,
        });
        assert!(matches!(
            core.tick(&mut succeeding).unwrap(),
            TickOutcome::Acked(_)
        ));
        assert_eq!(
            succeeding.applies,
            vec![(OpKind::Archive, "t1".to_string())],
            "the requeued operation must actually reach the provider"
        );
        assert_eq!(op_status(&core).0, "acked");
        assert!(archived(&core));
    }

    /// T-081's sixth acceptance test, stated as a mutation-proof
    /// regression: two identical commands dispatched back to back (before
    /// any `tick`) must still produce exactly one operation row, the same
    /// guarantee `store::tests::archive_is_idempotent_in_queue` already
    /// covers for the plain (never-failed) path. Kept here too because
    /// this file's `enqueue` revival path (`failed` -> `pending`) is new
    /// surface that could plausibly break it if it matched more than just
    /// `status = 'failed'`.
    #[test]
    fn two_dispatches_before_any_tick_still_yield_one_operation() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        archive(&mut core);
        archive(&mut core);
        assert_eq!(op_count(&core), 1);
        assert_eq!(op_status(&core).0, "pending");
    }

    /// T-087: the Diagnostics page's "Pending operations" field must tell
    /// a stuck-forever `failed` mark apart from one merely waiting its
    /// turn, and must not count a `running` row as either -- see
    /// `queue_counts`'s own doc comment for why. One row of each status
    /// is seeded directly (only `tick`/`tick_for_account` drive a row
    /// through `running`/`failed` naturally, and this test wants all
    /// three states present at once without wiring up a `MailProvider`).
    #[test]
    fn queue_counts_splits_pending_and_failed_and_ignores_running() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        archive(&mut core); // one 'pending' row on t1

        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
             VALUES ('t2', 'john', 'inbox', 'Hello', 'Hi', ?1, 1)",
            params![FIXTURE_NOW],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO operations
                 (id, account_id, target_id, op, payload, payload_hash, created_at,
                  retry_count, next_attempt_at, status)
             VALUES ('op-failed', 'john', 't2', 'archive', '{}', 'h2', ?1, 1, NULL, 'failed')",
            params![FIXTURE_NOW],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
             VALUES ('t3', 'john', 'inbox', 'Hello', 'Hi', ?1, 1)",
            params![FIXTURE_NOW],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO operations
                 (id, account_id, target_id, op, payload, payload_hash, created_at,
                  retry_count, next_attempt_at, status)
             VALUES ('op-running', 'john', 't3', 'archive', '{}', 'h3', ?1, 0, NULL, 'running')",
            params![FIXTURE_NOW],
        )
        .unwrap();
        // A second, distinct `pending` row (t4) so `pending` (2) and
        // `failed` (1) are not equal -- a `pending`/`failed` field swap
        // in `queue_counts` must fail this assertion, not pass it by
        // coincidence the way a 1-vs-1 count would.
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
             VALUES ('t4', 'john', 'inbox', 'Hello', 'Hi', ?1, 1)",
            params![FIXTURE_NOW],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO operations
                 (id, account_id, target_id, op, payload, payload_hash, created_at,
                  retry_count, next_attempt_at, status)
             VALUES ('op-pending-2', 'john', 't4', 'archive', '{}', 'h4', ?1, 0, NULL, 'pending')",
            params![FIXTURE_NOW],
        )
        .unwrap();

        let counts = core.queue_counts().unwrap();
        assert_eq!(
            counts.pending, 2,
            "the t1 archive from `archive()` above, plus op-pending-2"
        );
        assert_eq!(counts.failed, 1, "op-failed, and op-failed alone");
    }

    // --- T-084: a non-retryable `CreateFolder` failure must not leave a
    // ghost -- the row stays (see `Core::create_folder`'s doc comment for
    // why deleting it is not safe), but the sidebar must be able to tell
    // "not created on the server, never will be" apart from "pending" or
    // "confirmed."

    fn folder_row_exists(core: &Core, id: &str) -> bool {
        core.db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM folders WHERE id = ?1",
                params![id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            > 0
    }

    /// The observable behavior the acceptance test in T-084's plan entry
    /// asks for: after a terminal failure, `list_folders` -- what the
    /// sidebar actually reads -- must say this folder is not on the server
    /// and never will be, not just quietly keep it around unlabeled.
    #[test]
    fn unsupported_create_folder_marks_the_folder_not_created_instead_of_deleting_it() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let id = core
            .create_folder(&AccountId("john".into()), "Ideas")
            .unwrap();

        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Failed {
                error: ApplyError::Unsupported,
                ..
            }
        ));

        assert!(
            folder_row_exists(&core, id.as_str()),
            "T-084 rejects deleting the row (see Core::create_folder's doc comment); \
             a delete here would be the ghost bug in a different shape"
        );
        let folders = core.list_folders(&AccountId("john".into())).unwrap();
        let ghost = folders
            .iter()
            .find(|f| f.folder.id == id)
            .expect("the folder is still listed, not silently dropped");
        assert!(
            ghost.folder.create_failed,
            "sidebar must be told this folder is not on the server and never will be"
        );
    }

    /// The other half of the flag's contract, and the half that was
    /// missing: `create_failed` must be false for a folder whose *own*
    /// create is fine, even while another folder in the same account has a
    /// failed one. Without this, dropping the `o.target_id = f.id`
    /// correlation from `FOLDER_SUMMARY_SQL` would badge every unconfirmed
    /// folder in the account the moment any one of them failed -- and a
    /// folder that is being created perfectly well would tell the user it
    /// will never exist on the server. Verified by mutation: removing that
    /// line silently passed every test before this one existed.
    #[test]
    fn one_folders_failed_create_does_not_brand_another_folders() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let doomed = core
            .create_folder(&AccountId("john".into()), "Ideas")
            .unwrap();
        let innocent = core
            .create_folder(&AccountId("john".into()), "Recipes")
            .unwrap();

        // Only the first queued create is attempted, and it fails
        // terminally; the second is still sitting in the queue, unattempted
        // and unconfirmed -- exactly the state a badge must not fire on.
        let mut provider = AlwaysFailProvider(ApplyError::Unsupported);
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Failed {
                error: ApplyError::Unsupported,
                ..
            }
        ));

        let folders = core.list_folders(&AccountId("john".into())).unwrap();
        let flag = |id: &FolderId| {
            folders
                .iter()
                .find(|f| &f.folder.id == id)
                .unwrap_or_else(|| panic!("{} is still listed", id.as_str()))
                .folder
                .create_failed
        };
        assert!(flag(&doomed), "this is the one that actually failed");
        assert!(
            !flag(&innocent),
            "its own create is still pending -- saying it will never exist is a lie"
        );
    }

    /// And the flag must come from a failed *create*, not from any failed
    /// operation that happens to name this folder. An `Archive` into a
    /// folder can fail on its own (T-081 already rolls that back) without
    /// saying anything whatever about whether the folder exists on the
    /// server. Verified by mutation: removing `o.op = 'create_folder'`
    /// from `FOLDER_SUMMARY_SQL` silently passed every test before this
    /// one existed.
    #[test]
    fn an_unrelated_failed_operation_does_not_mark_a_folder_uncreated() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let id = core
            .create_folder(&AccountId("john".into()), "Ideas")
            .unwrap();

        // A failed operation that targets this same folder row but is not
        // its create. Written straight into `operations` because the
        // failure is the fixture here, not the thing under test.
        core.db
            .conn()
            .execute(
                "INSERT INTO operations
                     (id, account_id, target_id, op, payload, payload_hash,
                      created_at, retry_count, next_attempt_at, status)
                 VALUES ('op-unrelated', 'john', ?1, 'move', '{}', 'h', ?2, 0, NULL, 'failed')",
                params![id.as_str(), FIXTURE_NOW],
            )
            .unwrap();

        let folders = core.list_folders(&AccountId("john".into())).unwrap();
        let folder = folders
            .iter()
            .find(|f| f.folder.id == id)
            .expect("the folder is listed");
        assert!(
            !folder.folder.create_failed,
            "a failed move says nothing about whether the folder was created"
        );
    }

    /// A provider that refuses only operations of one `OpKind` and accepts
    /// everything else -- lets a test put a `CreateFolder` failure and a
    /// `Move` success in the same run, which `AlwaysFailProvider` and
    /// `FakeProvider` can't do on their own.
    struct FailOnlyKind {
        fail_kind: OpKind,
        error: ApplyError,
    }

    impl MailProvider for FailOnlyKind {
        fn apply(&mut self, op: &Operation) -> Result<(), ApplyError> {
            if op.kind == self.fail_kind {
                Err(self.error)
            } else {
                Ok(())
            }
        }
    }

    fn seed_custom_folder(core: &Core, id: &str, remote: &str, name: &str, delim: Option<&str>) {
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, remote_id, name, kind, delimiter)
                 VALUES (?1, 'john', ?2, ?3, 'custom', ?4)",
                params![id, remote, name, delim],
            )
            .unwrap();
    }

    fn folder_row(core: &Core, id: &str) -> (String, Option<String>) {
        core.db
            .conn()
            .query_row(
                "SELECT name, remote_id FROM folders WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    /// T-060t: the display name moves at once, the folder's *identity*
    /// only when the server confirms. Between the two, every other
    /// operation still resolves to the mailbox that actually exists.
    #[test]
    fn rename_folder_moves_the_label_now_and_the_identity_on_ack() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_custom_folder(&core, "john:ideas", "Ideas", "Ideas", Some("/"));
        let account = AccountId("john".into());

        core.rename_folder(&account, &FolderId("john:ideas".into()), "Plans")
            .unwrap();
        assert_eq!(
            folder_row(&core, "john:ideas"),
            ("Plans".to_string(), Some("Ideas".to_string())),
            "identity must not move before the wire ACK"
        );
        let payload: String = core
            .db
            .conn()
            .query_row(
                "SELECT payload FROM operations WHERE op = 'rename_folder'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(payload.contains(r#""from":"Ideas""#), "{payload}");
        assert!(payload.contains(r#""label":"Plans""#), "{payload}");
        assert_eq!(rename_destination(&payload).as_deref(), Some("Plans"));

        let mut provider = FakeProvider::ok();
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Acked(_)
        ));
        assert_eq!(
            folder_row(&core, "john:ideas"),
            ("Plans".to_string(), Some("Plans".to_string())),
            "the ACK is what moves remote_id"
        );
    }

    /// A nested mailbox keeps its path. Silently promoting `Team/Ideas` to
    /// a top-level `Plans` is the kind of move a user finds out about weeks
    /// later, when mail stops arriving where they filed it.
    #[test]
    fn rename_folder_keeps_a_nested_mailbox_in_its_hierarchy() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_custom_folder(&core, "john:team-ideas", "Team/Ideas", "Ideas", Some("/"));
        core.rename_folder(
            &AccountId("john".into()),
            &FolderId("john:team-ideas".into()),
            "Plans",
        )
        .unwrap();
        let payload: String = core
            .db
            .conn()
            .query_row(
                "SELECT payload FROM operations WHERE op = 'rename_folder'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            payload.contains(r#""parent_remote_id":"Team""#),
            "{payload}"
        );
        assert!(payload.contains(r#""delimiter":"/""#), "{payload}");
        assert!(payload.contains(r#""label":"Plans""#), "{payload}");
        assert_eq!(
            rename_destination(&payload).as_deref(),
            Some("Team/Plans"),
            "the parts must rejoin into the path the mailbox actually has"
        );

        let mut provider = FakeProvider::ok();
        core.tick(&mut provider).unwrap();
        assert_eq!(
            folder_row(&core, "john:team-ideas").1,
            Some("Team/Plans".to_string())
        );
    }

    /// T-158: a folder under a non-ASCII parent. `LIST` reported the parent
    /// already encoded (`&BB8EQAQ+BDUEOgRCBEs-` is «Проекты»), so joining
    /// that prefix to the new label and encoding the whole thing escaped
    /// the prefix a second time -- `&-BB8…`, a mailbox no server has. Only
    /// the leaf may be encoded, and `folders.remote_id` after the ACK must
    /// be exactly the path that went out.
    #[test]
    fn a_rename_under_a_non_ascii_parent_encodes_only_the_leaf() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_custom_folder(
            &core,
            "john:reports",
            "&BB8EQAQ+BDUEOgRCBEs-/Reports",
            "Reports",
            Some("/"),
        );

        core.rename_folder(
            &AccountId("john".into()),
            &FolderId("john:reports".into()),
            "Отчёты",
        )
        .unwrap();
        let payload: String = core
            .db
            .conn()
            .query_row(
                "SELECT payload FROM operations WHERE op = 'rename_folder'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let destination = rename_destination(&payload).expect("a rename has a destination");
        assert_eq!(destination, "&BB8EQAQ+BDUEOgRCBEs-/&BB4EQgRHBFEEQgRL-");
        assert!(
            !destination.contains("&-"),
            "an escaped ampersand means the parent was encoded twice: {destination}"
        );

        let mut provider = FakeProvider::ok();
        core.tick(&mut provider).unwrap();
        assert_eq!(
            folder_row(&core, "john:reports").1.as_deref(),
            Some(destination.as_str()),
            "the local id must be the mailbox the server was asked for"
        );
    }

    /// An operation queued before T-158 changed the payload carries a
    /// ready-made `to` and no `label`. It is still in the queue after the
    /// upgrade -- offline work survives restarts (D29) -- and must still
    /// settle, by the old route.
    #[test]
    fn a_rename_queued_with_the_old_payload_still_settles() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_custom_folder(&core, "john:ideas", "Team/Ideas", "Plans", Some("/"));
        core.db
            .conn()
            .execute(
                "INSERT INTO operations
                     (id, account_id, target_id, op, payload, payload_hash, created_at,
                      retry_count, status, seq)
                 VALUES ('rename_folder:john:john:ideas:h', 'john', 'john:ideas',
                         'rename_folder',
                         '{\"from\":\"Team/Ideas\",\"to\":\"Team/Plans\",\"at\":0}',
                         'h', ?1, 0, 'pending', 1)",
                params![FIXTURE_NOW],
            )
            .unwrap();

        let mut provider = FakeProvider::ok();
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Acked(_)
        ));
        assert_eq!(
            folder_row(&core, "john:ideas").1,
            Some("Team/Plans".to_string()),
            "the old payload's own `to` is what identity moves to"
        );
    }

    /// A terminal `RENAME` failure must leave identity where it was. The
    /// local label stays wrong only until the next `LIST` walk, which finds
    /// the same `remote_id` and refreshes `name` from the server -- the
    /// self-healing that only works because identity never moved.
    #[test]
    fn a_failed_rename_never_moves_folder_identity() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_custom_folder(&core, "john:ideas", "Ideas", "Ideas", Some("/"));
        core.rename_folder(
            &AccountId("john".into()),
            &FolderId("john:ideas".into()),
            "Plans",
        )
        .unwrap();
        let mut provider = FailOnlyKind {
            fail_kind: OpKind::RenameFolder,
            error: ApplyError::Unsupported,
        };
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Failed { .. }
        ));
        assert_eq!(
            folder_row(&core, "john:ideas").1,
            Some("Ideas".to_string()),
            "a folder the server never renamed must keep its mailbox"
        );

        // And discovery puts the label back without any special case.
        core.sync_folders(
            &AccountId("john".into()),
            &[crate::remote::DiscoveredFolder {
                remote_id: "Ideas".into(),
                kind: crate::model::FolderKind::Custom,
                label: "Ideas".into(),
                parent_remote_id: None,
                delimiter: Some('/'),
            }],
        )
        .unwrap();
        assert_eq!(
            folder_row(&core, "john:ideas"),
            ("Ideas".to_string(), Some("Ideas".to_string()))
        );
    }

    /// T-060u, and the mirror image of the rename rule above: the folder
    /// disappears from the sidebar at once, but keeps its `remote_id` --
    /// its identity -- until the server confirms the mailbox is gone. A
    /// `LIST` walk in that window has to find this row and adopt it, not
    /// insert a second folder for the mailbox that still exists.
    #[test]
    fn delete_folder_keeps_identity_until_the_ack_then_drops_it() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_custom_folder(&core, "john:ideas", "Team/Ideas", "Ideas", Some("/"));
        let account = AccountId("john".into());

        assert!(core
            .delete_folder(&account, &FolderId("john:ideas".into()))
            .unwrap());
        assert_eq!(
            folder_row(&core, "john:ideas").1,
            Some("Team/Ideas".to_string()),
            "identity must not move before the wire ACK"
        );

        let mut provider = FakeProvider::ok();
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Acked(_)
        ));
        assert_eq!(
            folder_row(&core, "john:ideas").1,
            None,
            "the ACK is what releases the mailbox name"
        );
    }

    /// The `remote_id` release has to be the same idempotent, payload-guarded
    /// update as `settle_folder_rename`'s: if the row has moved on to a
    /// different mailbox since this operation was queued, the ACK for the
    /// *old* mailbox must not strip the new one's identity.
    #[test]
    fn a_delete_ack_for_a_stale_mailbox_leaves_the_current_one_alone() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_custom_folder(&core, "john:ideas", "Ideas", "Ideas", Some("/"));
        core.delete_folder(&AccountId("john".into()), &FolderId("john:ideas".into()))
            .unwrap();
        core.db
            .conn()
            .execute(
                "UPDATE folders SET remote_id = 'Plans' WHERE id = 'john:ideas'",
                [],
            )
            .unwrap();

        let mut provider = FakeProvider::ok();
        core.tick(&mut provider).unwrap();
        assert_eq!(
            folder_row(&core, "john:ideas").1,
            Some("Plans".to_string()),
            "the payload names the mailbox this ACK is about, and only it"
        );
    }

    /// A terminal `DELETE` failure -- the applier's emptiness guard firing,
    /// most likely -- leaves a tombstone for a mailbox that is still there.
    /// Nothing special-cases it: the next `LIST` finds the mailbox by the
    /// `remote_id` the failure never released, and the folder comes back
    /// with its mail. Same self-healing as a failed rename, same reason.
    #[test]
    fn a_failed_delete_lets_the_next_list_bring_the_folder_back() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_custom_folder(&core, "john:ideas", "Ideas", "Ideas", Some("/"));
        let account = AccountId("john".into());
        core.delete_folder(&account, &FolderId("john:ideas".into()))
            .unwrap();
        let mut provider = FailOnlyKind {
            fail_kind: OpKind::DeleteFolder,
            error: ApplyError::NotEmpty,
        };
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Failed { .. }
        ));
        assert_eq!(
            folder_row(&core, "john:ideas").1,
            Some("Ideas".to_string()),
            "a mailbox the server never deleted must keep its identity"
        );
        assert!(
            !core
                .list_folders(&account)
                .unwrap()
                .iter()
                .any(|f| f.folder.id.as_str() == "john:ideas"),
            "until discovery runs, the user's request still stands"
        );

        core.sync_folders(
            &account,
            &[crate::remote::DiscoveredFolder {
                remote_id: "Ideas".into(),
                kind: crate::model::FolderKind::Custom,
                label: "Ideas".into(),
                parent_remote_id: None,
                delimiter: Some('/'),
            }],
        )
        .unwrap();
        assert!(
            core.list_folders(&account)
                .unwrap()
                .iter()
                .any(|f| f.folder.id.as_str() == "john:ideas"),
            "the mailbox is still on the server, so the folder is still real"
        );
    }

    /// The other half of that rule, and the more dangerous one: while a
    /// `DELETE` is still *in flight*, a `LIST` will of course still report
    /// the mailbox -- it has not been deleted yet. Reviving on that would
    /// undo the user's request the moment a sync ran, and the ACK would
    /// then leave a live-looking folder with no `remote_id`.
    #[test]
    fn a_list_during_an_in_flight_delete_does_not_revive_the_folder() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_custom_folder(&core, "john:ideas", "Ideas", "Ideas", Some("/"));
        let account = AccountId("john".into());
        core.delete_folder(&account, &FolderId("john:ideas".into()))
            .unwrap();
        core.sync_folders(
            &account,
            &[crate::remote::DiscoveredFolder {
                remote_id: "Ideas".into(),
                kind: crate::model::FolderKind::Custom,
                label: "Ideas".into(),
                parent_remote_id: None,
                delimiter: Some('/'),
            }],
        )
        .unwrap();
        assert!(
            !core
                .list_folders(&account)
                .unwrap()
                .iter()
                .any(|f| f.folder.id.as_str() == "john:ideas"),
            "a pending deletion outranks a LIST that has not seen it yet"
        );
        assert_eq!(
            folder_row(&core, "john:ideas").1,
            Some("Ideas".to_string()),
            "adoption still has to keep identity pointing at the mailbox"
        );
    }

    /// D29 dedup keys on kind+account+target+payload hash, so a rename back
    /// to a name this folder already carried would otherwise hash to the
    /// earlier *acked* row and be dropped by `INSERT OR IGNORE` -- the
    /// server would keep the middle name forever. The queue-time stamp in
    /// the payload is what keeps the third rename a real operation.
    #[test]
    fn renaming_back_and_forth_still_queues_every_hop() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        seed_custom_folder(&core, "john:ideas", "Ideas", "Ideas", Some("/"));
        let account = AccountId("john".into());
        let folder = FolderId("john:ideas".into());
        let mut provider = FakeProvider::ok();
        for (step, name) in ["Plans", "Ideas", "Plans"].iter().enumerate() {
            core.set_now(FIXTURE_NOW + step as i64);
            core.rename_folder(&account, &folder, name).unwrap();
            assert!(
                matches!(core.tick(&mut provider).unwrap(), TickOutcome::Acked(_)),
                "hop {step} to {name} produced no operation"
            );
        }
        assert_eq!(
            folder_row(&core, "john:ideas"),
            ("Plans".to_string(), Some("Plans".to_string()))
        );
        let renames: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE op = 'rename_folder'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(renames, 3);
    }

    /// The scenario that makes rejecting option (a) concrete: the user
    /// moves mail into a folder before its `CreateFolder` op has even been
    /// picked up, and the `Move` itself is confirmed by the server (so
    /// T-081 has nothing left to roll back for it) while `CreateFolder`
    /// later fails non-retryably. `apply_one`'s `Move` arm has no notion of
    /// "unconfirmed folder" -- it only requires the row to exist, and it
    /// already does. A rollback that deleted the folder row on the
    /// `CreateFolder` failure would then either violate the
    /// `threads.folder_id` foreign key (this crate runs with
    /// `PRAGMA foreign_keys = ON`, `feathermail-db`, D13) or, if that FK
    /// ever grew `ON DELETE CASCADE`, take the already-acked thread down
    /// with it. Leaving the row alone is what keeps the user's mail
    /// exactly where the server and the user both agree it is.
    #[test]
    fn unsupported_create_folder_does_not_strand_mail_already_moved_into_it() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let id = core
            .create_folder(&AccountId("john".into()), "Ideas")
            .unwrap();
        core.dispatch(Command::Move {
            account_id: AccountId("john".into()),
            thread_ids: vec![ThreadId("t1".into())],
            folder_id: id.clone(),
        })
        .unwrap();
        assert_eq!(raw_folder_id(&core, "t1"), id.as_str());

        // Two operations are now queued for this account: CreateFolder
        // (enqueued first, so claimed first) and Move. CreateFolder is
        // rejected non-retryably; Move is accepted and acked.
        let mut provider = FailOnlyKind {
            fail_kind: OpKind::CreateFolder,
            error: ApplyError::Unsupported,
        };
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Failed { .. }
        ));
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Acked(_)
        ));

        assert!(
            folder_row_exists(&core, id.as_str()),
            "the folder the thread now lives in must still exist"
        );
        assert_eq!(
            raw_folder_id(&core, "t1"),
            id.as_str(),
            "the thread must still be exactly where the user (and the server) put it -- \
             not stranded by a folder row that vanished out from under it"
        );
    }

    #[test]
    fn running_undo_stays_blocked_until_original_ack() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let receipt = core
            .dispatch_with_receipt(Command::MarkRead {
                account_id: AccountId("john".into()),
                thread_ids: vec![ThreadId("t1".into())],
            })
            .unwrap();
        let original = receipt.operations.first().unwrap().operation_id.clone();
        core.db
            .conn()
            .execute(
                "UPDATE operations SET status = 'running' WHERE id = ?1",
                params![original.as_str()],
            )
            .unwrap();
        let reverse = match core
            .undo(&crate::store::UndoTicket {
                operation_id: original.clone(),
            })
            .unwrap()
        {
            crate::store::UndoReceipt::ReverseQueued {
                reverse_operation_id,
                ..
            } => reverse_operation_id,
            other => panic!("expected blocked reverse, got {other:?}"),
        };
        let status: String = core
            .db
            .conn()
            .query_row(
                "SELECT status FROM operations WHERE id = ?1",
                params![reverse.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "blocked");
        core.finish(&original, OpStatus::Acked, None, None).unwrap();
        let released: String = core
            .db
            .conn()
            .query_row(
                "SELECT status FROM operations WHERE id = ?1",
                params![reverse.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(released, "pending");
    }

    #[test]
    fn terminal_original_failure_cancels_its_blocked_reverse() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let receipt = core
            .dispatch_with_receipt(Command::MarkRead {
                account_id: AccountId("john".into()),
                thread_ids: vec![ThreadId("t1".into())],
            })
            .unwrap();
        let original = receipt.operations.first().unwrap().operation_id.clone();
        let claimed = core
            .claim_next(FIXTURE_NOW, None)
            .unwrap()
            .expect("dispatch must create a claimable operation");
        assert_eq!(claimed.id, original);
        let reverse = match core
            .undo(&crate::store::UndoTicket {
                operation_id: original.clone(),
            })
            .unwrap()
        {
            crate::store::UndoReceipt::ReverseQueued {
                reverse_operation_id,
                ..
            } => reverse_operation_id,
            other => panic!("expected blocked reverse, got {other:?}"),
        };
        core.fail_and_undo(&claimed, 1).unwrap();
        let status: String = core
            .db
            .conn()
            .query_row(
                "SELECT status FROM operations WHERE id = ?1",
                params![reverse.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
    }

    fn raw_folder_id(core: &Core, thread_id: &str) -> String {
        core.db
            .conn()
            .query_row(
                "SELECT folder_id FROM threads WHERE id = ?1",
                params![thread_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Control for the two tests above: a `CreateFolder` that the provider
    /// actually accepts must not carry the "not created" flag forward.
    #[test]
    fn acked_create_folder_is_not_marked_create_failed() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let id = core
            .create_folder(&AccountId("john".into()), "Ideas")
            .unwrap();
        let mut provider = FakeProvider::ok();
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Acked(_)
        ));
        let folders = core.list_folders(&AccountId("john".into())).unwrap();
        let created = folders.iter().find(|f| f.folder.id == id).unwrap();
        assert!(!created.folder.create_failed);
    }

    /// Star -> Unstar -> Star: the third command must reach the wire. The
    /// D29 idempotency key dedups *unsent* work, not a state the user has
    /// legitimately returned to.
    #[test]
    fn a_command_repeated_after_its_ack_is_queued_again() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let acct = AccountId("john".into());
        let t1 = vec![ThreadId("t1".into())];
        let mut provider = FakeProvider::ok();
        core.dispatch(Command::Star {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        core.tick(&mut provider).unwrap();
        core.dispatch(Command::Unstar {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        core.tick(&mut provider).unwrap();
        core.dispatch(Command::Star {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        assert_eq!(
            core.queue_counts().unwrap().pending,
            1,
            "re-starring must queue an operation, not vanish"
        );
        core.tick(&mut provider).unwrap();
        assert_eq!(
            provider.applies.len(),
            3,
            "the third command must reach the provider"
        );
    }

    /// The same rule for the flag users toggle most: read -> unread -> read.
    #[test]
    fn mark_read_after_mark_unread_after_an_ack_is_queued_again() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let acct = AccountId("john".into());
        let t1 = vec![ThreadId("t1".into())];
        let mut provider = FakeProvider::ok();
        for cmd in [
            Command::MarkRead {
                account_id: acct.clone(),
                thread_ids: t1.clone(),
            },
            Command::MarkUnread {
                account_id: acct.clone(),
                thread_ids: t1.clone(),
            },
        ] {
            core.dispatch(cmd).unwrap();
            core.tick(&mut provider).unwrap();
        }
        core.dispatch(Command::MarkRead {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        assert_eq!(core.queue_counts().unwrap().pending, 1);
    }

    /// Two commands on one thread inside the same wall-clock second must
    /// apply in the order the user issued them, not alphabetically by op
    /// name.
    #[test]
    fn two_commands_in_one_second_apply_in_dispatch_order() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let acct = AccountId("john".into());
        let t1 = vec![ThreadId("t1".into())];
        core.dispatch(Command::Star {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        core.dispatch(Command::Archive {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        let mut provider = FakeProvider::ok();
        core.tick(&mut provider).unwrap();
        core.tick(&mut provider).unwrap();
        let kinds: Vec<OpKind> = provider.applies.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            vec![OpKind::Star, OpKind::Archive],
            "FIFO within one second, or a flag set before a move is lost"
        );
    }

    /// The mirror case, where the alphabet points the other way
    /// ("mark_read" > "archive"): still dispatch order, not luck.
    #[test]
    fn mark_read_before_archive_in_one_second_still_applies_first() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let acct = AccountId("john".into());
        let t1 = vec![ThreadId("t1".into())];
        core.dispatch(Command::MarkRead {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        core.dispatch(Command::Archive {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        let mut provider = FakeProvider::ok();
        core.tick(&mut provider).unwrap();
        core.tick(&mut provider).unwrap();
        let kinds: Vec<OpKind> = provider.applies.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, vec![OpKind::MarkRead, OpKind::Archive]);
    }

    /// T-162: a command the user issues again lands on the row the first
    /// one wrote (D29's idempotency key) and `enqueue` revives that row in
    /// place. It re-enters the queue *now* though, not where it stood when
    /// it was first issued -- under the old `rowid` tie-break the revived
    /// Star was claimed ahead of the Unstar issued between the two, so the
    /// server ended up with the flag the user had just taken off while
    /// SQLite showed it set.
    #[test]
    fn a_revived_operation_applies_after_the_commands_issued_before_it() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let acct = AccountId("john".into());
        let t1 = vec![ThreadId("t1".into())];
        let mut provider = FakeProvider::ok();

        core.dispatch(Command::Star {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        core.tick(&mut provider).unwrap();

        // Same second, so `created_at` ties on all three and the tie-break
        // is the whole question.
        core.dispatch(Command::Unstar {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        core.dispatch(Command::Star {
            account_id: acct.clone(),
            thread_ids: t1.clone(),
        })
        .unwrap();
        assert_eq!(
            core.queue_counts().unwrap().pending,
            2,
            "precondition: the second Star revived the acked row, it did not \
             open a third one"
        );

        core.tick(&mut provider).unwrap();
        core.tick(&mut provider).unwrap();

        let kinds: Vec<OpKind> = provider.applies.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            vec![OpKind::Star, OpKind::Unstar, OpKind::Star],
            "the revived Star goes out after the Unstar issued before it, or \
             the last state the user asked for is not the one the server keeps"
        );
    }

    /// The regression that reviving `acked` rows is most likely to cause:
    /// D29 must still collapse the *same unsent* command into one operation.
    #[test]
    fn the_same_command_twice_before_any_tick_is_still_one_operation() {
        let mut core = Core::memory().unwrap();
        core.set_now(FIXTURE_NOW);
        seed(&core);
        let acct = AccountId("john".into());
        let t1 = vec![ThreadId("t1".into())];
        for _ in 0..2 {
            core.dispatch(Command::Star {
                account_id: acct.clone(),
                thread_ids: t1.clone(),
            })
            .unwrap();
        }
        assert_eq!(core.queue_counts().unwrap().pending, 1);
        let mut provider = FakeProvider::ok();
        core.tick(&mut provider).unwrap();
        assert_eq!(core.tick(&mut provider).unwrap(), TickOutcome::Idle);
        assert_eq!(provider.applies.len(), 1);
    }
}
