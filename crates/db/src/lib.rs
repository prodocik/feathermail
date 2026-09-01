//! SQLite schema, migrations, and FTS.
//!
//! UI and MCP talk to Core, not this crate (D9). The GTK shell still reads
//! FakeMailStore; Core (T-007) writes here.

use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, Transaction, TransactionBehavior};

/// Schema version written by [`Database::open`].
pub const SCHEMA_VERSION: i32 = 29;

/// Inbox page query. T-005/D15 require its folder-local index even when
/// another account-wide ordering index exists for FTS search, so the SQL pins
/// `threads_account_folder_date` rather than leaving that contract to the
/// cost planner.
pub const INBOX_PAGE_SQL: &str = "\
SELECT id, subject, snippet, date, unread, starred, has_attachment, message_count
FROM threads INDEXED BY threads_account_folder_date
WHERE account_id = ?1
  AND folder_id = ?2
  AND archived = 0
  AND deleted = 0
  AND snooze_until IS NULL
ORDER BY date DESC, id DESC
LIMIT 64";

/// Tables required by T-005 (plus `schema_migrations` and `messages_fts`).
pub const REQUIRED_TABLES: &[&str] = &[
    "accounts",
    "folders",
    "threads",
    "messages",
    "attachments",
    "labels",
    "message_labels",
    "drafts",
    "draft_attachments",
    "snoozes",
    "sync_state",
    "operations",
    "operation_moves",
    "operation_move_history",
    "settings",
    "search_history",
    "mcp_clients",
    "mcp_permissions",
    "mcp_confirmation_requests",
    "mcp_audit",
    "outbox",
    "outbox_attachments",
    "fts_pending",
    "fts_message_rows",
];

#[cfg(test)]
const FORBIDDEN_COLUMNS: &[&str] = &[
    "password",
    "token",
    "secret",
    "oauth",
    "access_token",
    "refresh_token",
];

/// `~/.local/share/feathermail/mail.db` (D13). Honors `XDG_DATA_HOME`.
pub fn default_db_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("feathermail").join("mail.db")
}

#[derive(Debug)]
pub enum Error {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    /// A migration's own self-check failed (e.g. `PRAGMA foreign_key_check`
    /// after the T-079 `folders` rebuild). Distinct from `Sqlite` so it
    /// reads as "our migration logic is wrong", not "SQLite rejected a
    /// statement".
    Migration(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Migration(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Migration(_) => None,
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        chmod_owner_rw(path)?;
        let db = Self { conn };
        db.configure(true)?;
        db.migrate()?;
        Ok(db)
    }

    pub fn memory() -> Result<Self> {
        let db = Self {
            conn: Connection::open_in_memory()?,
        };
        db.configure(false)?;
        db.migrate()?;
        Ok(db)
    }

    fn configure(&self, wal: bool) -> Result<()> {
        self.conn.pragma_update(None, "foreign_keys", "ON")?;
        self.conn.pragma_update(None, "busy_timeout", 5000)?;
        if wal {
            self.enable_wal()?;
            self.conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        Ok(())
    }

    /// Switch the file to WAL, tolerating a concurrent opener doing the same.
    ///
    /// `PRAGMA journal_mode = WAL` on a file that is not in WAL yet needs
    /// an exclusive lock, and SQLite answers `SQLITE_BUSY` at once without
    /// consulting the busy handler, so `busy_timeout` does not help here.
    /// Two `Database::open` racing on a fresh profile (the GUI starting
    /// while an agent spawns the stdio MCP server, or the worker's
    /// per-connect `Core::open`) would otherwise lose one of them with
    /// "database is locked" -- and the shell then quietly falls back to an
    /// ephemeral in-memory session. Retry with a growing pause, and accept
    /// the file already being in WAL: whoever won switched it for everyone.
    /// A file that is in WAL already never takes the lock, so the steady
    /// state costs nothing.
    fn enable_wal(&self) -> Result<()> {
        const ATTEMPTS: u32 = 8;
        let mut last_err = None;
        for attempt in 0..ATTEMPTS {
            match self.conn.pragma_update(None, "journal_mode", "WAL") {
                Ok(()) => return Ok(()),
                Err(err) => {
                    let mode: String = self
                        .conn
                        .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
                    if mode.eq_ignore_ascii_case("wal") {
                        return Ok(());
                    }
                    last_err = Some(err);
                    // 10 ms, 20, 40, ... 1280: about 2.5 s in total, well
                    // inside the 5 s `busy_timeout` the rest of open uses.
                    std::thread::sleep(std::time::Duration::from_millis(10 << attempt));
                }
            }
        }
        Err(last_err.expect("at least one attempt was made").into())
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(include_str!("schema.sql"))?;
        let current: i32 = self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;
        if current < 2 {
            add_column_if_missing(&self.conn, "accounts", "imap_security", "TEXT")?;
            add_column_if_missing(&self.conn, "accounts", "smtp_security", "TEXT")?;
        }
        if current < 3 {
            // T-022 second half: resumable newest-first backfill cursor
            // (feathermail_sync::FolderSyncState::backfill_floor/target).
            // Additive and idempotent: a profile already at v3 (or a brand
            // new one, whose `sync_state` already has these columns from
            // schema.sql) just finds both columns present and skips.
            add_column_if_missing(&self.conn, "sync_state", "backfill_floor", "INTEGER")?;
            add_column_if_missing(&self.conn, "sync_state", "backfill_target", "INTEGER")?;
        }
        if current < 4 {
            // T-024: cache-limit sweep needs to SUM() cached body sizes
            // without stat()-ing every file on disk (see schema.sql doc on
            // the column). Additive and idempotent, same shape as the v3
            // block above.
            add_column_if_missing(&self.conn, "messages", "body_bytes", "INTEGER")?;
        }
        if current < 5 {
            // T-079: `folders` had `UNIQUE (account_id, name)`, but folder
            // identity is `remote_id`, not the display name -- two real
            // mailboxes in different branches (`Team/Ideas`,
            // `Personal/Ideas`) can share a display name. Unlike the v2->v3
            // and v3->v4 blocks above, this is not additive: SQLite has no
            // `ALTER TABLE ... DROP CONSTRAINT`/`ALTER COLUMN`, so changing
            // which columns a UNIQUE constraint covers needs the
            // documented rebuild (see `rebuild_folders_table_for_v5`'s doc
            // comment for why it's safe under `foreign_keys = ON`, D13).
            // Idempotent the same way as the additive blocks: a profile
            // already at v5 (or a brand new one, whose `folders` already
            // has this shape from `schema.sql`) has nothing this needs to
            // change, but the rebuild runs unconditionally inside this
            // `current < 5` guard rather than re-checking the constraint
            // shape, so it must itself tolerate running against an
            // already-current `folders` table without losing anything --
            // which it does, since it is just a copy-drop-rename of
            // whatever rows and columns already exist.
            // ...but only where it is actually needed. The rebuild replaces
            // `folders` with a definition hardcoded in Rust, and a brand
            // new profile already got the right shape from `schema.sql` a
            // few lines above. Rebuilding it there would quietly promote
            // the Rust copy to the real definition of the table: the next
            // column added to `folders` in `schema.sql` would exist on
            // every upgraded profile and silently vanish on every new one.
            if folders_has_legacy_unique(&self.conn)? {
                rebuild_folders_table_for_v5(&self.conn)?;
            }
        }
        if current < 6 {
            // T-081: `operations.undo_payload` holds the pre-mutation
            // snapshot `feathermail_core::store` captures when it queues an
            // operation, so a non-retryable provider failure can put the
            // local mark back instead of leaving SQLite asserting a state
            // the server never confirmed (see schema.sql's doc on the
            // column). Purely additive, same shape as the v2/v3/v4 blocks
            // above -- unlike the v5 `folders` rebuild, nothing about this
            // column's meaning depends on rows already in the table.
            //
            // Uses the existing-column guard rather than a bare `ALTER
            // TABLE ... ADD COLUMN` precisely because this `current < 6`
            // block also runs against a brand-new profile (whose `current`
            // starts at 0): `schema.sql` already created `operations` with
            // `undo_payload` on that profile, and a raw `ALTER TABLE ADD
            // COLUMN` would fail with "duplicate column name" there (the
            // defect T-079 already hit once for `folders`, see the v5
            // block's doc comment).
            add_column_if_missing(&self.conn, "operations", "undo_payload", "TEXT")?;
        }
        if current < 7 {
            // T-078 (b) prep: `feathermail_sync::schedule::next_sync` needs
            // `last_attempt_at`/`consecutive_failures` per folder (see
            // schema.sql's doc on these columns, and
            // `FolderInput::last_attempt_at` in
            // crates/sync/src/schedule.rs), and neither survived a restart
            // before this. Purely additive, same shape as the v2/v3/v4/v6
            // blocks above -- unlike the v5 `folders` rebuild, nothing about
            // these columns' meaning depends on rows already in the table.
            //
            // Existing-column guard for the same reason as the v6 block:
            // this also runs against a brand-new profile (`current` starts
            // at 0), whose `sync_state` already has both columns from
            // `schema.sql`, so a bare `ALTER TABLE ... ADD COLUMN` would
            // fail with "duplicate column name" there.
            add_column_if_missing(&self.conn, "sync_state", "last_attempt_at", "INTEGER")?;
            add_column_if_missing(
                &self.conn,
                "sync_state",
                "consecutive_failures",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        if current < 8 {
            // T-048: `fts_pending` (see schema.sql's doc comment on it) is
            // brand new, so `IF NOT EXISTS` is enough here -- unlike the
            // v5 `folders` rebuild, nothing about its shape depends on what
            // a pre-v8 profile already has, so there is no "already
            // current, don't re-touch it" case to guard against the way
            // the v6/v7 blocks above do for existing columns.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS fts_pending (
                    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
                    queued_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS fts_pending_queued_at ON fts_pending (queued_at);",
            )?;
            // Every message that already existed before this upgrade has
            // never been through `messages_fts` -- nothing wrote to it
            // before T-048 (see `messages_fts`'s doc comment in
            // schema.sql). Without this backfill, a profile upgraded from
            // an older version would only ever index *new* mail from here
            // on, leaving every message synced before the upgrade
            // permanently unsearchable. `INSERT OR IGNORE` makes this safe
            // to run unconditionally the same way the table create above
            // is: a brand-new profile has no rows in `messages` yet, so
            // this is a no-op there, and it can never collide with a
            // message this same `current < 8` block's table-create just
            // made queueable for the first time.
            self.conn.execute(
                "INSERT OR IGNORE INTO fts_pending (message_id, queued_at)
                 SELECT id, strftime('%s','now') FROM messages",
                [],
            )?;
        }
        if current < 9 {
            // T-093: before this, `index_one` wrote raw undecoded RFC822
            // bytes into `messages_fts.body` instead of parsed text, so
            // every message indexed before this fix is findable only by
            // header/MIME-boundary noise, not by its actual words. The fix
            // itself only changes what future indexing writes -- it does
            // nothing for rows `messages_fts` already holds from before the
            // fix, so those stay wrong forever unless something re-queues
            // them. This block is that something: it re-queues every
            // message that has a cached body (`body_path IS NOT NULL`) so
            // `Core::index_pending_batch` re-indexes it with the corrected
            // parser. `WHERE body_path IS NOT NULL` is deliberate, not an
            // optimization -- a message with no cached body indexes to an
            // empty string either way (see `body_text_for_index`), so
            // queuing it here would burn a background-indexer pass for a
            // no-op rewrite. `INSERT OR IGNORE` mirrors the `current < 8`
            // block above: safe to run unconditionally, since a brand-new
            // profile has no rows in `messages` yet and this can never
            // collide with a row the `current < 8` block above just queued
            // in the same upgrade.
            self.conn.execute(
                "INSERT OR IGNORE INTO fts_pending (message_id, queued_at)
                 SELECT id, strftime('%s','now') FROM messages
                 WHERE body_path IS NOT NULL",
                [],
            )?;
        }
        if current < 10 {
            // T-076: one durable row per message moved by our own queued
            // operation.  The source locator remains usable until the
            // server ACKs (and until source sync observes it vanished),
            // while destination sync can rehome the same local message row
            // instead of manufacturing a second logical message/thread.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS operation_moves (
                    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
                    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                    source_folder_id TEXT NOT NULL REFERENCES folders(id),
                    source_remote_id TEXT NOT NULL,
                    source_uid INTEGER NOT NULL,
                    destination_folder_id TEXT NOT NULL REFERENCES folders(id),
                    destination_remote_id TEXT NOT NULL,
                    destination_uid INTEGER,
                    PRIMARY KEY (operation_id, message_id),
                    UNIQUE (operation_id, source_folder_id, source_uid),
                    UNIQUE (operation_id, destination_folder_id, destination_uid)
                );
                CREATE INDEX IF NOT EXISTS operation_moves_source_locator
                    ON operation_moves (source_folder_id, source_uid);
                CREATE INDEX IF NOT EXISTS operation_moves_destination_locator
                    ON operation_moves (destination_folder_id, destination_uid);
                CREATE INDEX IF NOT EXISTS operation_moves_message
                    ON operation_moves (message_id);
                CREATE UNIQUE INDEX IF NOT EXISTS messages_account_folder_provider_uid
                    ON messages (account_id, folder_id, provider_uid)
                    WHERE provider_uid IS NOT NULL;",
            )?;
        }
        if current < 11 {
            // T-034: Undo is a durable state transition. `blocked` reverse
            // rows wait for the original wire operation; `cancelled` rows
            // preserve the original idempotency/audit record after a
            // pre-ACK Undo. `undo_of` is the causal link and deliberately
            // remains on operations rather than being inferred from ids.
            add_column_if_missing(
                &self.conn,
                "operations",
                "undo_of",
                "TEXT REFERENCES operations(id)",
            )?;
            add_column_if_missing(&self.conn, "operations", "undo_requested_at", "INTEGER")?;
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS operation_move_history (
                    history_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    operation_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    source_folder_id TEXT NOT NULL,
                    source_remote_id TEXT NOT NULL,
                    source_uid INTEGER NOT NULL,
                    destination_folder_id TEXT NOT NULL,
                    destination_remote_id TEXT NOT NULL,
                    destination_uid INTEGER,
                    recorded_at INTEGER NOT NULL,
                    UNIQUE (operation_id, message_id, source_folder_id,
                            source_uid, destination_folder_id,
                            destination_uid)
                );
                CREATE INDEX IF NOT EXISTS operation_move_history_operation
                    ON operation_move_history (operation_id, history_id);
                CREATE INDEX IF NOT EXISTS operation_move_history_message
                    ON operation_move_history (message_id, history_id);",
            )?;
        }
        if current < 12 {
            // T-041/T-045: outbox is a full immutable snapshot of the
            // draft handed to Send. The operation queue only carries the
            // outbox id; bodies therefore never appear in an Operation's
            // Debug output or payload, and editing the original draft
            // cannot mutate an already queued message.
            add_column_if_missing(
                &self.conn,
                "outbox",
                "from_addr",
                "TEXT NOT NULL DEFAULT ''",
            )?;
            add_column_if_missing(&self.conn, "outbox", "cc", "TEXT NOT NULL DEFAULT ''")?;
            add_column_if_missing(&self.conn, "outbox", "bcc", "TEXT NOT NULL DEFAULT ''")?;
            add_column_if_missing(&self.conn, "outbox", "body", "TEXT NOT NULL DEFAULT ''")?;
            add_column_if_missing(&self.conn, "outbox", "in_reply_to", "TEXT")?;
            add_column_if_missing(&self.conn, "outbox", "references_header", "TEXT")?;
            add_column_if_missing(&self.conn, "outbox", "sent_at", "INTEGER")?;
        }
        if current < 13 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS draft_attachments (
                    id TEXT PRIMARY KEY,
                    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                    draft_id TEXT NOT NULL REFERENCES drafts(id) ON DELETE CASCADE,
                    filename TEXT NOT NULL,
                    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
                    size_bytes INTEGER NOT NULL,
                    source_path TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS draft_attachments_draft
                    ON draft_attachments (draft_id, id);
                CREATE TABLE IF NOT EXISTS outbox_attachments (
                    outbox_id TEXT NOT NULL REFERENCES outbox(id) ON DELETE CASCADE,
                    filename TEXT NOT NULL,
                    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
                    size_bytes INTEGER NOT NULL,
                    source_path TEXT NOT NULL,
                    PRIMARY KEY (outbox_id, filename, source_path)
                );",
            )?;
        }
        if current < 14 {
            // T-042: an autosave can happen more than once in a single
            // wall-clock second, so `drafts.updated_at` cannot safely be
            // the queue's revision. This separate, additive counter lets
            // an in-flight older APPEND observe that it became stale and
            // ACK without uploading the newer content twice.
            add_column_if_missing(
                &self.conn,
                "drafts",
                "sync_revision",
                "INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        if current < 15 {
            // T-043: attachment downloads fetch the individual IMAP MIME
            // section, not a second full RFC822 message. Existing attachment
            // rows predate lazy download metadata; NULL `part_path` leaves
            // them visible but honestly not downloadable until the body is
            // fetched again and parsed.
            add_column_if_missing(&self.conn, "attachments", "part_path", "TEXT")?;
            add_column_if_missing(
                &self.conn,
                "attachments",
                "transfer_encoding",
                "TEXT NOT NULL DEFAULT 'identity'",
            )?;
        }
        if current < 16 {
            // T-046: Reply all needs the original Cc separately from To.
            // Existing messages predate this header and keep an empty Cc;
            // the next metadata sync fills it without altering recipients.
            add_column_if_missing(&self.conn, "messages", "cc", "TEXT NOT NULL DEFAULT ''")?;
        }
        if current < 17 {
            // T-059: MCP client policy is durable Core state.  A process
            // environment may only narrow that policy, never grant it.  The
            // request table is the one cross-process hand-off between the
            // local stdio server and the user's GTK window; it deliberately
            // stores opaque ids and revisions, never arguments or mail data.
            add_column_if_missing(
                &self.conn,
                "mcp_clients",
                "permission_level",
                "TEXT NOT NULL DEFAULT 'draft'",
            )?;
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS mcp_confirmation_requests (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    client_id TEXT NOT NULL REFERENCES mcp_clients(id) ON DELETE CASCADE,
                    capability TEXT NOT NULL,
                    account_id TEXT REFERENCES accounts(id) ON DELETE CASCADE,
                    target_id TEXT,
                    fingerprint TEXT NOT NULL,
                    status TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    expires_at INTEGER NOT NULL,
                    resolved_at INTEGER
                );
                CREATE INDEX IF NOT EXISTS mcp_confirmation_pending
                    ON mcp_confirmation_requests (status, created_at, id);
                CREATE INDEX IF NOT EXISTS mcp_confirmation_fingerprint
                    ON mcp_confirmation_requests
                        (client_id, capability, fingerprint, status);",
            )?;
        }
        if current < 18 {
            // T-060l: a batch delete confirmation must show a safe count,
            // while its exact opaque target set stays only in the digest.
            // Existing single-target rows gain the truthful default of one.
            add_column_if_missing(
                &self.conn,
                "mcp_confirmation_requests",
                "target_count",
                "INTEGER NOT NULL DEFAULT 1",
            )?;
        }
        if current < 19 {
            // T-068: `messages_fts.message_id` is deliberately UNINDEXED,
            // so deleting one old FTS row by that value scans the whole
            // virtual table. Keep a normal SQLite primary-key map to the
            // FTS5 rowid instead. The schema is installed above for new
            // profiles; this one-pass join backfills existing v18 profiles
            // before any writer starts relying on the map. Joining through
            // `messages` intentionally skips any historical FTS orphan:
            // the map only represents live message rows, and its FK then
            // removes the mapping when that message is deleted.
            self.conn.execute(
                "INSERT OR IGNORE INTO fts_message_rows (message_id, fts_rowid)
                 SELECT f.message_id, f.rowid
                 FROM messages_fts f
                 JOIN messages m ON m.id = f.message_id",
                [],
            )?;
        }
        if current < 20 {
            // T-060s: `sync_requests` (see schema.sql's doc comment on it)
            // is brand new, so `IF NOT EXISTS` is enough here, the same way
            // it was for `fts_pending` in the v8 block -- nothing about its
            // shape depends on what a pre-v20 profile already has, and it
            // starts empty on every profile because a request only means
            // "someone asked for a sync since the shell last looked".
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS sync_requests (
                    account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
                    requested_at INTEGER NOT NULL
                );",
            )?;
        }
        if current < 21 {
            // T-060t: renaming a nested folder has to keep its path, and the
            // path separator is server-reported data Core cannot recover
            // from `remote_id` alone. Existing rows keep NULL until the next
            // LIST walk fills them in, which is exactly the honest state:
            // "we have not been told yet."
            add_column_if_missing(&self.conn, "folders", "delimiter", "TEXT")?;
        }
        if current < 22 {
            // T-060u: a deleted folder is a hidden row, not a removed one --
            // `messages` and the durable Undo history reference folders by
            // id. Existing rows are all present, so NULL is right for them.
            add_column_if_missing(&self.conn, "folders", "deleted_at", "INTEGER")?;
        }
        if current < 23 {
            // T-096: the same shape of stale-index problem the v9 block
            // above fixed, one content type further along. Until now an
            // HTML-only message (no `text/plain` alternative) was indexed
            // with an empty `body`, so it was findable by sender and
            // subject but never by a word it contained. The fix changes
            // only what future indexing writes; rows `messages_fts`
            // already holds stay empty forever unless something re-queues
            // them, and for mail that arrived months ago nothing ever
            // would.
            //
            // Every message with a cached body is re-queued, not just the
            // HTML-only ones: `messages` records no content type, so
            // "which rows are affected" is not a question this schema can
            // answer -- only the MIME bytes on disk can, and reading them
            // is exactly the work `Core::index_pending_batch` is for.
            // Re-indexing a plain-text message is idempotent and cheap
            // (T-092's worker drains the queue 200 at a time in the
            // background), so the honest move is to re-queue broadly
            // rather than to guess narrowly and leave some mail
            // unfindable with no way to tell which.
            self.conn.execute(
                "INSERT OR IGNORE INTO fts_pending (message_id, queued_at)
                 SELECT id, strftime('%s','now') FROM messages
                 WHERE body_path IS NOT NULL",
                [],
            )?;
        }
        if current < 24 {
            // T-102: "what was opened stays cached for a couple of days."
            // The size sweep ordered candidates by `messages.date`, so the
            // oldest mail went first -- including a ten-year-old thread the
            // owner opened a minute ago, which then had to be fetched again.
            // Nothing in the schema recorded a read, so add the column;
            // existing rows are honestly NULL ("never opened by us"), which
            // sorts them ahead of anything read recently, exactly as before.
            add_column_if_missing(&self.conn, "messages", "body_read_at", "INTEGER")?;
        }
        if current < 25 {
            // T-111: the attachment cache had no budget at all -- only
            // bodies were ever swept -- so a profile that downloaded
            // attachments grew without a ceiling. The sweep needs to know
            // what each cached file costs; `size_bytes` cannot answer that
            // (it is the server's count for the *encoded* part, a third
            // larger than the base64-decoded file on disk). Rows cached
            // before this column existed stay NULL and are counted at
            // `size_bytes` instead, which is the only number this profile
            // has for them -- see `Core::enforce_attachment_cache_limit`.
            add_column_if_missing(&self.conn, "attachments", "cache_bytes", "INTEGER")?;
        }
        if current < 26 {
            // T-134: the ampersand runs. `html_to_text` collected entities
            // inside tags, so every `&` in an image URL's tracking
            // parameters landed in the preview -- "&&&&&&&&" and then the
            // letter. The parser is fixed; these rows were written by the
            // broken one and nothing recomputes a stored snippet on its
            // own. Only the damaged-looking rows are queued: unlike the
            // v9/v23 re-index blocks above, "which rows are affected" *is*
            // a question this schema can answer -- the artefact is in the
            // stored text. A row whose `&&` turns out to be genuine is
            // visited once, left as it is, and dropped from the queue.
            self.conn.execute(
                "INSERT OR IGNORE INTO snippet_repairs (message_id)
                 SELECT id FROM messages
                 WHERE body_path IS NOT NULL AND snippet LIKE '%&&%'",
                [],
            )?;
        }
        if current < 27 {
            // T-141: the reading pane used to show the `text/plain` half of
            // a `multipart/alternative` by default (Fork D-plain-default),
            // which is why a Jira notification read "Логотип Jira
            // [https://...png]" instead of drawing the logo. The default is
            // now the sanitized HTML alternative, and the stored value has
            // to follow it on profiles that already exist: `settings` rows
            // are written on every flush whether the reader touched the
            // toggle or not, so a profile made before this holds the old
            // *default*, not a decision. The toggle itself is untouched --
            // Settings -> Privacy still turns plain text back on, and the
            // value it writes then survives every later open.
            self.conn.execute(
                "UPDATE settings SET value = 'false' \
                 WHERE key = 'prefer_plain' AND value = 'true'",
                [],
            )?;
        }
        if current < 28 {
            // Two `REFERENCES` clauses were the only ones in this schema
            // left at the SQL default, `NO ACTION`: `drafts.thread_id` and
            // `outbox.draft_id`. Both named a parent that ordinary use
            // deletes underneath them, so both turned a routine delete into
            // `FOREIGN KEY constraint failed`:
            //
            //  * sync deletes threads (a UIDVALIDITY reset, the last
            //    message of a thread vanishing on the server, a duplicate
            //    being folded away). One reply draft was enough to roll the
            //    whole sync transaction back -- deterministically, on every
            //    later poll, so the folder never synced again.
            //  * `Core::delete_draft` deletes a draft that has already been
            //    handed to the outbox. The discard came back "Couldn't save
            //    that change." and the row could never be removed.
            //
            // Both become `ON DELETE SET NULL` (see schema.sql for why that
            // and not CASCADE: a draft is the owner's own text, and an
            // outbox row is a frozen snapshot per T-041/T-045 -- neither
            // may ride its parent out). Like the v5 `folders` rebuild this
            // is not additive: SQLite cannot alter a foreign key in place,
            // so the tables are copied, dropped and renamed, and for the
            // same reason as there the rebuild is entered only when the
            // stored table text says it is needed -- a brand new profile
            // already got the right shape from `schema.sql`, and rebuilding
            // it anyway would quietly promote this Rust copy to the real
            // definition of both tables.
            if draft_links_lack_on_delete_set_null(&self.conn)? {
                rebuild_draft_links_for_v28(&self.conn)?;
            }
        }
        if current < 29 {
            // Three additive changes that share one version bump.
            //
            // T-157: `sync_state.resync_cursor` / `resync_completed_at` --
            // the rolling reconciliation walk that finally lets a message
            // deleted below the newest window disappear locally on a
            // CONDSTORE server too (schema.sql carries the argument). Both
            // are NULL on every existing row, which is the honest state:
            // "no walk has started, and none has ever finished", so the
            // first pass after the upgrade starts one.
            add_column_if_missing(&self.conn, "sync_state", "resync_cursor", "INTEGER")?;
            add_column_if_missing(&self.conn, "sync_state", "resync_completed_at", "INTEGER")?;
            // T-162: `operations.seq`. Existing rows are backfilled from
            // `rowid`, which is exactly the order they were inserted in and
            // therefore the order `claim_next` was already applying them in
            // -- the upgrade changes nothing about a queue that is already
            // drained, and everything about the next revive.
            add_column_if_missing(&self.conn, "operations", "seq", "INTEGER")?;
            self.conn
                .execute("UPDATE operations SET seq = rowid WHERE seq IS NULL", [])?;
            // T-155: the FTS text normalisation changed, so every row
            // `messages_fts` already holds answers a different set of
            // queries than a row indexed today. Same move as the v9 and
            // v23 blocks above, one difference: *every* message is
            // re-queued, not only those with a cached body. Normalisation
            // applies to the subject and the addresses as well, and those
            // are indexed for a message whose body was never downloaded --
            // filtering on `body_path` here would leave exactly that mail
            // findable only by the old rules, with nothing to tell the
            // owner which half of the mailbox that is.
            self.conn.execute(
                "INSERT OR IGNORE INTO fts_pending (message_id, queued_at)
                 SELECT id, strftime('%s','now') FROM messages",
                [],
            )?;
        }
        if current < SCHEMA_VERSION {
            self.conn.execute(
                "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, strftime('%s','now'))",
                [SCHEMA_VERSION],
            )?;
        }
        Ok(())
    }

    /// Core runs statements. UI and MCP must not import rusqlite (D9).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Starts a write transaction before policy validation.  Core uses this
    /// for high-risk operations whose authorization and mutation must share
    /// one SQLite linearization point.
    pub fn immediate_transaction(&mut self) -> rusqlite::Result<Transaction<'_>> {
        self.conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
    }

    /// Same write transaction through a shared borrow, for the many Core
    /// doorways that only hold `&Database`.
    ///
    /// Needed because `busy_timeout` does not cover the DEFERRED
    /// read-then-write shape under WAL: once such a transaction has taken a
    /// read snapshot, a competing commit makes the upgrade to a write fail
    /// with `SQLITE_BUSY_SNAPSHOT` (extended code 517) *immediately* -- the
    /// busy handler is never consulted, so the 5 s timeout D13 configures
    /// buys nothing. Beginning IMMEDIATE takes the write lock before the
    /// first read, which is a case `busy_timeout` does cover.
    ///
    /// `Transaction::new_unchecked` is what makes the shared borrow legal;
    /// the `&mut self` on [`Self::immediate_transaction`] is only rusqlite's
    /// compile-time guard against nesting. Nesting here is a runtime error
    /// ("cannot start a transaction within a transaction") instead, so use
    /// this only where no transaction is already open on the handle.
    ///
    /// Not a blanket replacement: IMMEDIATE holds the write lock for the
    /// whole transaction, so read-only paths (search, list) must stay
    /// DEFERRED rather than block every writer for the length of a SELECT.
    pub fn immediate_transaction_ref(&self) -> rusqlite::Result<Transaction<'_>> {
        Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
    }

    pub fn schema_version(&self) -> Result<i32> {
        let v = self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?;
        Ok(v)
    }

    pub fn journal_mode(&self) -> Result<String> {
        let mode: String = self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        Ok(mode)
    }

    pub fn explain_query_plan(&self, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Result<String> {
        let mut stmt = self.conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        let lines = stmt
            .query_map(params, |row| {
                let detail: String = row.get(3)?;
                Ok(detail)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(lines.join("\n"))
    }

    /// Tables (other than `accounts` itself) that carry an `account_id`
    /// column — the sweep list for a full account wipe (T-021). Computed
    /// from `sqlite_master` / `PRAGMA table_info`, not a hardcoded list, so
    /// a future table with an `account_id` column is picked up on its own.
    pub fn tables_with_account_id(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for table in self.table_names()? {
            if table == "accounts" {
                continue;
            }
            let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let has_account_id = stmt
                .query_map([], |row| row.get::<_, String>(1))?
                .filter_map(std::result::Result::ok)
                .any(|name| name == "account_id");
            if has_account_id {
                out.push(table);
            }
        }
        Ok(out)
    }

    pub fn table_names(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT name FROM sqlite_master
             WHERE type IN ('table', 'view')
               AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )?;
        let names = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(names)
    }

    #[cfg(test)]
    fn column_names(&self) -> Result<Vec<(String, String)>> {
        let tables = self.table_names()?;
        let mut out = Vec::new();
        for table in tables {
            let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let cols = stmt
                .query_map([], |row| {
                    let name: String = row.get(1)?;
                    Ok(name)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for col in cols {
                out.push((table.clone(), col));
            }
        }
        Ok(out)
    }
}

fn add_column_if_missing(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(std::result::Result::ok)
        .any(|name| name == column);
    if !exists {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
    }
    Ok(())
}

/// Does this profile's `folders` table still carry the pre-T-079
/// `UNIQUE (account_id, name)`? Read off `sqlite_master`'s stored `CREATE
/// TABLE` text (whitespace-normalised, since SQLite stores it verbatim as
/// it was written) rather than off `schema_migrations`: the version
/// number says which migrations have been *recorded*, this says what the
/// table actually looks like right now, which is the only thing the
/// rebuild cares about.
fn folders_has_legacy_unique(conn: &Connection) -> Result<bool> {
    let sql: String = conn.query_row(
        "SELECT COALESCE(\
           (SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'folders'), '')",
        [],
        |row| row.get(0),
    )?;
    let normalised = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    Ok(normalised.contains("UNIQUE (account_id, name)"))
}

/// T-079: rewrites `folders`'s UNIQUE constraint from `(account_id, name)`
/// to `(account_id, remote_id)`, plus a `parent_id`-scoped index on `name`
/// (`schema.sql`'s `folders_account_parent_name`). SQLite has no `ALTER
/// TABLE` for changing which columns a UNIQUE constraint covers
/// (<https://sqlite.org/lang_altertable.html>, "Making Other Kinds Of
/// Table Schema Changes"), so this follows that page's documented
/// twelve-step recipe: build the new shape under a temporary name, copy
/// every row across untouched, drop the old table, rename the new one
/// into its place, then rebuild the indexes that were dropped along with
/// the old table.
///
/// `folders.id` is the FK target of `threads.folder_id` and
/// `messages.folder_id` (plain `REFERENCES`, no `ON DELETE CASCADE`), and
/// `folders.parent_id` self-references `folders.id`; this connection runs
/// with `foreign_keys = ON` (D13, `configure`). Enforcing FKs *during* the
/// rebuild would fail the `DROP TABLE folders` step the moment any thread
/// or message still points at it, so `foreign_keys` is switched OFF first
/// -- that pragma is a documented no-op inside a transaction, which is why
/// it happens before `BEGIN`, not inside it -- and back ON afterwards.
/// `PRAGMA foreign_key_check` right before `COMMIT` is the safety net the
/// SQLite docs call for in this exact situation: it walks every FK in the
/// database and would surface a mismatch (e.g. a bug in the copy step
/// that dropped a row) as a hard error instead of silently committing
/// orphaned threads or messages.
///
/// The self-reference is why `folders_new` is declared with `parent_id
/// TEXT REFERENCES folders(id)` -- naming the *final* table, not
/// `folders_new` itself. A foreign key clause is resolved by table name
/// at enforcement time, not bound to a specific table at `CREATE TABLE`
/// time, so while `folders_new` and the old `folders` briefly coexist
/// that text refers to the old table (irrelevant, since FK checks are
/// off); once the old table is dropped and `folders_new` is renamed to
/// `folders`, the very same unedited text now correctly refers to the
/// table itself. `threads.folder_id`/`messages.folder_id` need no
/// attention for the same reason: their `REFERENCES folders(id)` clauses
/// were never touched, and `folders` is the name both the old and the
/// rebuilt table answer to.
///
/// Runs whenever `current < 5` (see call site), including against a
/// freshly-created database whose `folders` table already has this exact
/// shape from `schema.sql` -- harmless (a copy-drop-rename of a table
/// that may be empty), just not the cheapest possible no-op; that only
/// happens once per profile; every later open finds `current == 5` and
/// skips this block entirely.
fn rebuild_folders_table_for_v5(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let outcome = (|| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE folders_new (
                 id TEXT PRIMARY KEY,
                 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 remote_id TEXT,
                 name TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 parent_id TEXT REFERENCES folders(id),
                 color TEXT,
                 UNIQUE (account_id, remote_id)
             );
             INSERT INTO folders_new (id, account_id, remote_id, name, kind, parent_id, color)
                 SELECT id, account_id, remote_id, name, kind, parent_id, color FROM folders;
             DROP TABLE folders;
             ALTER TABLE folders_new RENAME TO folders;
             CREATE INDEX IF NOT EXISTS folders_account ON folders (account_id);
             CREATE UNIQUE INDEX IF NOT EXISTS folders_account_parent_name
                 ON folders (account_id, parent_id, name);",
        )?;
        {
            let mut stmt = tx.prepare("PRAGMA foreign_key_check")?;
            let mut rows = stmt.query([])?;
            if rows.next()?.is_some() {
                return Err(Error::Migration(
                    "T-079 folders rebuild left a dangling foreign key (see \
                     rebuild_folders_table_for_v5)"
                        .to_string(),
                ));
            }
        }
        tx.commit()?;
        Ok(())
    })();
    // Whether the closure above committed or bailed out early (rolling the
    // transaction back via `Transaction::drop`), foreign key enforcement
    // must be restored either way.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    outcome
}

/// Do `drafts.thread_id` and `outbox.draft_id` still point at their parents
/// with a bare `REFERENCES` (i.e. `NO ACTION`)?
///
/// Read off `sqlite_master`'s stored `CREATE TABLE` text for the same
/// reason [`folders_has_legacy_unique`] does: `schema_migrations` records
/// which migrations ran, this says what the tables actually look like right
/// now, which is the only thing the rebuild below cares about. A profile
/// created from today's `schema.sql` already has both clauses and must be
/// left alone.
fn draft_links_lack_on_delete_set_null(conn: &Connection) -> Result<bool> {
    const EXPECTED: [(&str, &str); 2] = [
        (
            "drafts",
            "thread_id TEXT REFERENCES threads(id) ON DELETE SET NULL",
        ),
        (
            "outbox",
            "draft_id TEXT REFERENCES drafts(id) ON DELETE SET NULL",
        ),
    ];
    for (table, clause) in EXPECTED {
        let sql: String = conn.query_row(
            "SELECT COALESCE(\
               (SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1), '')",
            [table],
            |row| row.get(0),
        )?;
        let normalised = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        if !normalised.contains(clause) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// v27 -> v28: rebuilds `drafts` and `outbox` so their one non-cascading
/// foreign key each becomes `ON DELETE SET NULL` (see the `current < 28`
/// call site for what that fixes).
///
/// Follows [`rebuild_folders_table_for_v5`] step for step, including why it
/// is safe with `foreign_keys = ON` in force everywhere else (D13): the
/// pragma is switched off for the duration, so dropping a table other rows
/// still reference is allowed and -- crucially -- `DROP TABLE` does not run
/// the implicit `DELETE FROM` that would cascade `draft_attachments` away
/// with its parent. `draft_attachments.draft_id` and
/// `outbox_attachments.outbox_id` keep their unedited `REFERENCES drafts(id)`
/// / `REFERENCES outbox(id)` text, which points at whatever table answers to
/// that name -- the old one before the rename, the rebuilt one after it.
/// `PRAGMA foreign_key_check` on exactly the four tables involved proves
/// that before the transaction commits; it is deliberately scoped rather
/// than whole-database, so an unrelated pre-existing violation somewhere
/// else in an old profile cannot turn this into a database that refuses to
/// open at all.
fn rebuild_draft_links_for_v28(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let outcome = (|| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        // A profile that enforced NO ACTION cannot hold a draft pointing at
        // a thread that is already gone -- but one whose `drafts` table
        // predates the `REFERENCES` clause entirely (v13 and older) can.
        // Such a pointer is exactly what the new rule would have nulled at
        // deletion time, so null it now instead of letting
        // `foreign_key_check` reject the upgrade.
        tx.execute(
            "UPDATE drafts SET thread_id = NULL WHERE thread_id IS NOT NULL \
             AND thread_id NOT IN (SELECT id FROM threads)",
            [],
        )?;
        tx.execute(
            "UPDATE outbox SET draft_id = NULL WHERE draft_id IS NOT NULL \
             AND draft_id NOT IN (SELECT id FROM drafts)",
            [],
        )?;
        tx.execute_batch(
            "CREATE TABLE drafts_new (
                 id TEXT PRIMARY KEY,
                 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 thread_id TEXT REFERENCES threads(id) ON DELETE SET NULL,
                 in_reply_to TEXT,
                 from_addr TEXT NOT NULL,
                 to_addr TEXT NOT NULL DEFAULT '',
                 cc TEXT NOT NULL DEFAULT '',
                 bcc TEXT NOT NULL DEFAULT '',
                 subject TEXT NOT NULL DEFAULT '',
                 body TEXT NOT NULL DEFAULT '',
                 updated_at INTEGER NOT NULL,
                 remote_uid INTEGER,
                 sync_revision INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO drafts_new (id, account_id, thread_id, in_reply_to, from_addr,
                 to_addr, cc, bcc, subject, body, updated_at, remote_uid, sync_revision)
                 SELECT id, account_id, thread_id, in_reply_to, from_addr,
                     to_addr, cc, bcc, subject, body, updated_at, remote_uid, sync_revision
                 FROM drafts;
             DROP TABLE drafts;
             ALTER TABLE drafts_new RENAME TO drafts;
             CREATE TABLE outbox_new (
                 id TEXT PRIMARY KEY,
                 account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 draft_id TEXT REFERENCES drafts(id) ON DELETE SET NULL,
                 from_addr TEXT NOT NULL DEFAULT '',
                 to_addr TEXT NOT NULL,
                 cc TEXT NOT NULL DEFAULT '',
                 bcc TEXT NOT NULL DEFAULT '',
                 subject TEXT NOT NULL DEFAULT '',
                 body TEXT NOT NULL DEFAULT '',
                 in_reply_to TEXT,
                 references_header TEXT,
                 created_at INTEGER NOT NULL,
                 status TEXT NOT NULL DEFAULT 'queued',
                 sent_at INTEGER
             );
             INSERT INTO outbox_new (id, account_id, draft_id, from_addr, to_addr, cc, bcc,
                 subject, body, in_reply_to, references_header, created_at, status, sent_at)
                 SELECT id, account_id, draft_id, from_addr, to_addr, cc, bcc,
                     subject, body, in_reply_to, references_header, created_at, status, sent_at
                 FROM outbox;
             DROP TABLE outbox;
             ALTER TABLE outbox_new RENAME TO outbox;
             CREATE INDEX IF NOT EXISTS outbox_account_status ON outbox (account_id, status);",
        )?;
        for table in [
            "drafts",
            "draft_attachments",
            "outbox",
            "outbox_attachments",
        ] {
            let mut stmt = tx.prepare(&format!("PRAGMA foreign_key_check({table})"))?;
            let mut rows = stmt.query([])?;
            if rows.next()?.is_some() {
                return Err(Error::Migration(format!(
                    "v28 draft-link rebuild left a dangling foreign key in `{table}` \
                     (see rebuild_draft_links_for_v28)"
                )));
            }
        }
        tx.commit()?;
        Ok(())
    })();
    // Committed or bailed out early (rolling back via `Transaction::drop`),
    // foreign key enforcement must be restored either way.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    outcome
}

fn chmod_owner_rw(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Workspace probe so `cargo test -p feathermail-db` has a test.
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The real schema.sql this repo shipped at schema v4 -- i.e. before
    /// T-079 changed `folders`'s UNIQUE constraint -- captured verbatim via
    /// `git show HEAD:crates/db/src/schema.sql` (run against the commit
    /// immediately before that change, not the working tree, and not a
    /// checkout/stash of anything). Used by
    /// `migrate_upgrades_v4_profile_to_v5_rebuilding_folders_preserving_data`
    /// to build a genuine pre-T-079 profile by hand, so that test exercises
    /// the real rebuild path a real user's database would hit -- including
    /// the `remote_id`/`parent_id`/`color` columns on `folders` that a
    /// hand-reduced fixture might otherwise leave out (unlike the v2->v3
    /// and v3->v4 fixtures above, which never touched `folders`'s shape at
    /// all, this migration's whole point *is* `folders`'s shape).
    const OLD_SCHEMA_V4_SQL: &str = r#"
-- Feather Mail schema v1 (T-005). D13 WAL + file cache, D14 no secrets, D15 indexes, D31 queue.

CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT NOT NULL,
    provider TEXT NOT NULL,
    imap_host TEXT,
    imap_port INTEGER,
    smtp_host TEXT,
    smtp_port INTEGER,
    imap_security TEXT,
    smtp_security TEXT,
    username TEXT,
    status TEXT NOT NULL DEFAULT 'offline',
    download_policy TEXT NOT NULL DEFAULT 'recent',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS folders (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    remote_id TEXT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    parent_id TEXT REFERENCES folders(id),
    color TEXT,
    UNIQUE (account_id, name)
);

CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id),
    subject TEXT NOT NULL,
    snippet TEXT NOT NULL DEFAULT '',
    date INTEGER NOT NULL,
    unread INTEGER NOT NULL DEFAULT 0,
    starred INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    deleted INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    importance INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 1,
    snooze_until INTEGER,
    gm_thrid TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id),
    provider_uid INTEGER,
    message_id_header TEXT,
    in_reply_to TEXT,
    references_header TEXT,
    date INTEGER NOT NULL,
    sender_name TEXT NOT NULL DEFAULT '',
    sender_email TEXT NOT NULL DEFAULT '',
    recipients TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    snippet TEXT NOT NULL DEFAULT '',
    unread INTEGER NOT NULL DEFAULT 0,
    starred INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    importance INTEGER NOT NULL DEFAULT 0,
    body_path TEXT,
    size_bytes INTEGER NOT NULL DEFAULT 0,
    -- T-024 (schema v4): size in bytes of the *cached body file* at
    -- body_path, so the cache-limit sweep can SUM() this column instead of
    -- stat()-ing every file on disk. Distinct from size_bytes above, which
    -- is the server-reported RFC822.SIZE and means something else -- do not
    -- conflate the two. NULL until a body is actually cached.
    body_bytes INTEGER
);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes INTEGER NOT NULL DEFAULT 0,
    cache_path TEXT,
    content_id TEXT
);

CREATE TABLE IF NOT EXISTS labels (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    color TEXT,
    UNIQUE (account_id, name)
);

CREATE TABLE IF NOT EXISTS message_labels (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    label_id TEXT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (message_id, label_id)
);

CREATE TABLE IF NOT EXISTS drafts (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT REFERENCES threads(id),
    in_reply_to TEXT,
    from_addr TEXT NOT NULL,
    to_addr TEXT NOT NULL DEFAULT '',
    cc TEXT NOT NULL DEFAULT '',
    bcc TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    remote_uid INTEGER
);

CREATE TABLE IF NOT EXISTS snoozes (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    until_ts INTEGER NOT NULL,
    previous_folder_id TEXT NOT NULL,
    UNIQUE (account_id, thread_id)
);

CREATE TABLE IF NOT EXISTS sync_state (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uidvalidity INTEGER,
    uidnext INTEGER,
    highest_modseq INTEGER,
    last_sync_at INTEGER,
    -- T-022 second half (schema v3): resumable newest-first backfill cursor.
    -- See feathermail_sync::FolderSyncState for the exact semantics.
    backfill_floor INTEGER,
    backfill_target INTEGER,
    PRIMARY KEY (account_id, folder_id)
);

CREATE TABLE IF NOT EXISTS operations (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL,
    op TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    payload_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    status TEXT NOT NULL DEFAULT 'pending'
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS search_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    query TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_clients (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_permissions (
    client_id TEXT NOT NULL REFERENCES mcp_clients(id) ON DELETE CASCADE,
    capability TEXT NOT NULL,
    grant TEXT NOT NULL,
    PRIMARY KEY (client_id, capability)
);

CREATE TABLE IF NOT EXISTS mcp_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id TEXT,
    tool TEXT NOT NULL,
    account_id TEXT,
    target_id TEXT,
    outcome TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    draft_id TEXT REFERENCES drafts(id),
    to_addr TEXT NOT NULL,
    subject TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued'
);

CREATE INDEX IF NOT EXISTS folders_account ON folders (account_id);

CREATE INDEX IF NOT EXISTS threads_account_folder_date
    ON threads (account_id, folder_id, date DESC);
CREATE INDEX IF NOT EXISTS threads_account_unread
    ON threads (account_id, unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS threads_account_starred
    ON threads (account_id, starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS threads_snooze_until
    ON threads (snooze_until) WHERE snooze_until IS NOT NULL;

CREATE INDEX IF NOT EXISTS messages_account_thread
    ON messages (account_id, thread_id);
CREATE INDEX IF NOT EXISTS messages_account_provider_uid
    ON messages (account_id, provider_uid);
CREATE INDEX IF NOT EXISTS messages_message_id_header
    ON messages (message_id_header);
CREATE INDEX IF NOT EXISTS messages_account_folder_date
    ON messages (account_id, folder_id, date DESC);
CREATE INDEX IF NOT EXISTS messages_unread
    ON messages (unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS messages_starred
    ON messages (starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS messages_sender
    ON messages (account_id, sender_email);

CREATE INDEX IF NOT EXISTS snoozes_until ON snoozes (until_ts);
CREATE UNIQUE INDEX IF NOT EXISTS operations_idempotent
    ON operations (account_id, op, target_id, payload_hash)
    WHERE status IN ('pending', 'running');
CREATE INDEX IF NOT EXISTS search_history_created ON search_history (created_at DESC);
CREATE INDEX IF NOT EXISTS mcp_audit_created ON mcp_audit (created_at DESC);
CREATE INDEX IF NOT EXISTS outbox_account_status ON outbox (account_id, status);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5 (
    sender,
    recipients,
    subject,
    body,
    attachment_names,
    labels,
    message_id UNINDEXED,
    tokenize = 'unicode61'
);
"#;

    /// The real schema.sql this repo shipped at schema v5 -- i.e. before
    /// T-081 added `operations.undo_payload` -- captured verbatim via `git
    /// show HEAD:crates/db/src/schema.sql` (run against the commit
    /// immediately before that change, not the working tree). Used by
    /// `migrate_upgrades_v5_profile_to_v6_adding_undo_payload_preserving_data`
    /// to build a genuine pre-T-081 profile by hand, the same way
    /// `OLD_SCHEMA_V4_SQL` above does for the v4->v5 rebuild.
    const OLD_SCHEMA_V5_SQL: &str = r#"
-- Feather Mail schema v1 (T-005). D13 WAL + file cache, D14 no secrets, D15 indexes, D31 queue.

CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT NOT NULL,
    provider TEXT NOT NULL,
    imap_host TEXT,
    imap_port INTEGER,
    smtp_host TEXT,
    smtp_port INTEGER,
    imap_security TEXT,
    smtp_security TEXT,
    username TEXT,
    status TEXT NOT NULL DEFAULT 'offline',
    download_policy TEXT NOT NULL DEFAULT 'recent',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- T-079: folder identity is `remote_id` (the exact mailbox name the server's
-- `LIST` returned), not the display `name` -- two real mailboxes in
-- different branches (`Team/Ideas`, `Personal/Ideas`) can and do share a
-- display name. `remote_id` is NULL for a local-only folder not yet
-- reconciled with the server (`Core::create_folder`'s placeholder row,
-- T-077's adoption case); SQLite's UNIQUE allows any number of NULLs, so
-- multiple such rows coexisting is intended, not a gap. `name` is unique
-- only within one branch -- see `folders_account_parent_name` below, which
-- enforces that in `parent_id`'s stead since a plain column-level UNIQUE
-- can't express "unique per sibling group".
CREATE TABLE IF NOT EXISTS folders (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    remote_id TEXT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    parent_id TEXT REFERENCES folders(id),
    color TEXT,
    UNIQUE (account_id, remote_id)
);

CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id),
    subject TEXT NOT NULL,
    snippet TEXT NOT NULL DEFAULT '',
    date INTEGER NOT NULL,
    unread INTEGER NOT NULL DEFAULT 0,
    starred INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    deleted INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    importance INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 1,
    snooze_until INTEGER,
    gm_thrid TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id),
    provider_uid INTEGER,
    message_id_header TEXT,
    in_reply_to TEXT,
    references_header TEXT,
    date INTEGER NOT NULL,
    sender_name TEXT NOT NULL DEFAULT '',
    sender_email TEXT NOT NULL DEFAULT '',
    recipients TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    snippet TEXT NOT NULL DEFAULT '',
    unread INTEGER NOT NULL DEFAULT 0,
    starred INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    importance INTEGER NOT NULL DEFAULT 0,
    body_path TEXT,
    size_bytes INTEGER NOT NULL DEFAULT 0,
    -- T-024 (schema v4): size in bytes of the *cached body file* at
    -- body_path, so the cache-limit sweep can SUM() this column instead of
    -- stat()-ing every file on disk. Distinct from size_bytes above, which
    -- is the server-reported RFC822.SIZE and means something else -- do not
    -- conflate the two. NULL until a body is actually cached.
    body_bytes INTEGER
);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes INTEGER NOT NULL DEFAULT 0,
    cache_path TEXT,
    content_id TEXT
);

CREATE TABLE IF NOT EXISTS labels (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    color TEXT,
    UNIQUE (account_id, name)
);

CREATE TABLE IF NOT EXISTS message_labels (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    label_id TEXT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (message_id, label_id)
);

CREATE TABLE IF NOT EXISTS drafts (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT REFERENCES threads(id),
    in_reply_to TEXT,
    from_addr TEXT NOT NULL,
    to_addr TEXT NOT NULL DEFAULT '',
    cc TEXT NOT NULL DEFAULT '',
    bcc TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    remote_uid INTEGER
);

CREATE TABLE IF NOT EXISTS snoozes (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    until_ts INTEGER NOT NULL,
    previous_folder_id TEXT NOT NULL,
    UNIQUE (account_id, thread_id)
);

CREATE TABLE IF NOT EXISTS sync_state (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uidvalidity INTEGER,
    uidnext INTEGER,
    highest_modseq INTEGER,
    last_sync_at INTEGER,
    -- T-022 second half (schema v3): resumable newest-first backfill cursor.
    -- See feathermail_sync::FolderSyncState for the exact semantics.
    backfill_floor INTEGER,
    backfill_target INTEGER,
    PRIMARY KEY (account_id, folder_id)
);

CREATE TABLE IF NOT EXISTS operations (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL,
    op TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    payload_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    status TEXT NOT NULL DEFAULT 'pending'
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS search_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    query TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_clients (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_permissions (
    client_id TEXT NOT NULL REFERENCES mcp_clients(id) ON DELETE CASCADE,
    capability TEXT NOT NULL,
    grant TEXT NOT NULL,
    PRIMARY KEY (client_id, capability)
);

CREATE TABLE IF NOT EXISTS mcp_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id TEXT,
    tool TEXT NOT NULL,
    account_id TEXT,
    target_id TEXT,
    outcome TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    draft_id TEXT REFERENCES drafts(id),
    to_addr TEXT NOT NULL,
    subject TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued'
);

CREATE INDEX IF NOT EXISTS folders_account ON folders (account_id);
-- T-079: name uniqueness lives per sibling group, not per account, so two
-- real mailboxes under different parents (`Team/Ideas`, `Personal/Ideas`)
-- can both display as "Ideas". Deliberately a plain (not COALESCE'd)
-- multi-column index: SQLite treats `NULL != NULL` for UNIQUE purposes, so
-- two rows with `parent_id IS NULL` never collide on `name` here, same as
-- `remote_id IS NULL` never collides above. That is intentional, not an
-- oversight: `Core::sync_folders` links `parent_id` in a second pass after
-- every row in a `LIST` walk is written (see its doc comment), so a branch
-- whose own container folder was filtered out or simply hasn't been
-- discovered yet leaves its children's `parent_id` unset (top-level, by
-- this index's lights) -- e.g. three "Ideas" folders from three unrelated
-- branches, discovered without their parent containers, must still all
-- land as "Ideas" rather than fight over one root-level slot. Top-level
-- name collisions among folders a user actually creates locally are still
-- prevented -- one level up, in `Core::create_folder`'s own duplicate
-- check, not by this index.
CREATE UNIQUE INDEX IF NOT EXISTS folders_account_parent_name
    ON folders (account_id, parent_id, name);

CREATE INDEX IF NOT EXISTS threads_account_folder_date
    ON threads (account_id, folder_id, date DESC);
CREATE INDEX IF NOT EXISTS threads_account_unread
    ON threads (account_id, unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS threads_account_starred
    ON threads (account_id, starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS threads_snooze_until
    ON threads (snooze_until) WHERE snooze_until IS NOT NULL;

CREATE INDEX IF NOT EXISTS messages_account_thread
    ON messages (account_id, thread_id);
CREATE INDEX IF NOT EXISTS messages_account_provider_uid
    ON messages (account_id, provider_uid);
CREATE INDEX IF NOT EXISTS messages_message_id_header
    ON messages (message_id_header);
CREATE INDEX IF NOT EXISTS messages_account_folder_date
    ON messages (account_id, folder_id, date DESC);
CREATE INDEX IF NOT EXISTS messages_unread
    ON messages (unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS messages_starred
    ON messages (starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS messages_sender
    ON messages (account_id, sender_email);

CREATE INDEX IF NOT EXISTS snoozes_until ON snoozes (until_ts);
CREATE UNIQUE INDEX IF NOT EXISTS operations_idempotent
    ON operations (account_id, op, target_id, payload_hash)
    WHERE status IN ('pending', 'running');
CREATE INDEX IF NOT EXISTS search_history_created ON search_history (created_at DESC);
CREATE INDEX IF NOT EXISTS mcp_audit_created ON mcp_audit (created_at DESC);
CREATE INDEX IF NOT EXISTS outbox_account_status ON outbox (account_id, status);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5 (
    sender,
    recipients,
    subject,
    body,
    attachment_names,
    labels,
    message_id UNINDEXED,
    tokenize = 'unicode61'
);
"#;

    /// The real schema.sql this repo shipped at schema v6 -- i.e. before
    /// T-078 (b) prep added `sync_state.last_attempt_at`/
    /// `consecutive_failures` -- captured verbatim via `git show
    /// HEAD:crates/db/src/schema.sql` (run against the commit immediately
    /// before that change, not the working tree). Used by
    /// `migrate_upgrades_v6_profile_to_v7_adding_sync_attempt_tracking_preserving_data`
    /// to build a genuine pre-v7 profile by hand, the same way
    /// `OLD_SCHEMA_V5_SQL` above does for the v5->v6 upgrade.
    const OLD_SCHEMA_V6_SQL: &str = r#"
-- Feather Mail schema v1 (T-005). D13 WAL + file cache, D14 no secrets, D15 indexes, D31 queue.

CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT NOT NULL,
    provider TEXT NOT NULL,
    imap_host TEXT,
    imap_port INTEGER,
    smtp_host TEXT,
    smtp_port INTEGER,
    imap_security TEXT,
    smtp_security TEXT,
    username TEXT,
    status TEXT NOT NULL DEFAULT 'offline',
    download_policy TEXT NOT NULL DEFAULT 'recent',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- T-079: folder identity is `remote_id` (the exact mailbox name the server's
-- `LIST` returned), not the display `name` -- two real mailboxes in
-- different branches (`Team/Ideas`, `Personal/Ideas`) can and do share a
-- display name. `remote_id` is NULL for a local-only folder not yet
-- reconciled with the server (`Core::create_folder`'s placeholder row,
-- T-077's adoption case); SQLite's UNIQUE allows any number of NULLs, so
-- multiple such rows coexisting is intended, not a gap. `name` is unique
-- only within one branch -- see `folders_account_parent_name` below, which
-- enforces that in `parent_id`'s stead since a plain column-level UNIQUE
-- can't express "unique per sibling group".
CREATE TABLE IF NOT EXISTS folders (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    remote_id TEXT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    parent_id TEXT REFERENCES folders(id),
    color TEXT,
    UNIQUE (account_id, remote_id)
);

CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id),
    subject TEXT NOT NULL,
    snippet TEXT NOT NULL DEFAULT '',
    date INTEGER NOT NULL,
    unread INTEGER NOT NULL DEFAULT 0,
    starred INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    deleted INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    importance INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 1,
    snooze_until INTEGER,
    gm_thrid TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id),
    provider_uid INTEGER,
    message_id_header TEXT,
    in_reply_to TEXT,
    references_header TEXT,
    date INTEGER NOT NULL,
    sender_name TEXT NOT NULL DEFAULT '',
    sender_email TEXT NOT NULL DEFAULT '',
    recipients TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    snippet TEXT NOT NULL DEFAULT '',
    unread INTEGER NOT NULL DEFAULT 0,
    starred INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    importance INTEGER NOT NULL DEFAULT 0,
    body_path TEXT,
    size_bytes INTEGER NOT NULL DEFAULT 0,
    -- T-024 (schema v4): size in bytes of the *cached body file* at
    -- body_path, so the cache-limit sweep can SUM() this column instead of
    -- stat()-ing every file on disk. Distinct from size_bytes above, which
    -- is the server-reported RFC822.SIZE and means something else -- do not
    -- conflate the two. NULL until a body is actually cached.
    body_bytes INTEGER
);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes INTEGER NOT NULL DEFAULT 0,
    cache_path TEXT,
    content_id TEXT
);

CREATE TABLE IF NOT EXISTS labels (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    color TEXT,
    UNIQUE (account_id, name)
);

CREATE TABLE IF NOT EXISTS message_labels (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    label_id TEXT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (message_id, label_id)
);

CREATE TABLE IF NOT EXISTS drafts (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT REFERENCES threads(id),
    in_reply_to TEXT,
    from_addr TEXT NOT NULL,
    to_addr TEXT NOT NULL DEFAULT '',
    cc TEXT NOT NULL DEFAULT '',
    bcc TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    remote_uid INTEGER
);

CREATE TABLE IF NOT EXISTS snoozes (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    until_ts INTEGER NOT NULL,
    previous_folder_id TEXT NOT NULL,
    UNIQUE (account_id, thread_id)
);

CREATE TABLE IF NOT EXISTS sync_state (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uidvalidity INTEGER,
    uidnext INTEGER,
    highest_modseq INTEGER,
    last_sync_at INTEGER,
    -- T-022 second half (schema v3): resumable newest-first backfill cursor.
    -- See feathermail_sync::FolderSyncState for the exact semantics.
    backfill_floor INTEGER,
    backfill_target INTEGER,
    PRIMARY KEY (account_id, folder_id)
);

CREATE TABLE IF NOT EXISTS operations (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL,
    op TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    payload_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    status TEXT NOT NULL DEFAULT 'pending',
    -- T-081 (schema v6): the pre-mutation snapshot of the columns this
    -- operation's own command touched on `threads`, captured by
    -- `Core::dispatch` the instant before it wrote the optimistic local
    -- mark. Deliberately NOT folded into `payload`: `payload_hash` is D29's
    -- idempotency key, and mixing in the *previous* state would make two
    -- `Archive` calls on the same thread from different starting states
    -- hash to two different operations, breaking dedup. When a provider
    -- apply fails non-retryably (`ApplyError` other than `Network`, and
    -- other than `Conflict` which the queue treats as success),
    -- `feathermail_core::queue` reads this column and puts the mark back,
    -- so SQLite stops asserting a state the server never confirmed. NULL
    -- for operations that don't carry an undo (e.g. `create_folder`).
    undo_payload TEXT
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS search_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    query TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_clients (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_permissions (
    client_id TEXT NOT NULL REFERENCES mcp_clients(id) ON DELETE CASCADE,
    capability TEXT NOT NULL,
    grant TEXT NOT NULL,
    PRIMARY KEY (client_id, capability)
);

CREATE TABLE IF NOT EXISTS mcp_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id TEXT,
    tool TEXT NOT NULL,
    account_id TEXT,
    target_id TEXT,
    outcome TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    draft_id TEXT REFERENCES drafts(id),
    to_addr TEXT NOT NULL,
    subject TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued'
);

CREATE INDEX IF NOT EXISTS folders_account ON folders (account_id);
-- T-079: name uniqueness lives per sibling group, not per account, so two
-- real mailboxes under different parents (`Team/Ideas`, `Personal/Ideas`)
-- can both display as "Ideas". Deliberately a plain (not COALESCE'd)
-- multi-column index: SQLite treats `NULL != NULL` for UNIQUE purposes, so
-- two rows with `parent_id IS NULL` never collide on `name` here, same as
-- `remote_id IS NULL` never collides above. That is intentional, not an
-- oversight: `Core::sync_folders` links `parent_id` in a second pass after
-- every row in a `LIST` walk is written (see its doc comment), so a branch
-- whose own container folder was filtered out or simply hasn't been
-- discovered yet leaves its children's `parent_id` unset (top-level, by
-- this index's lights) -- e.g. three "Ideas" folders from three unrelated
-- branches, discovered without their parent containers, must still all
-- land as "Ideas" rather than fight over one root-level slot. Top-level
-- name collisions among folders a user actually creates locally are still
-- prevented -- one level up, in `Core::create_folder`'s own duplicate
-- check, not by this index.
CREATE UNIQUE INDEX IF NOT EXISTS folders_account_parent_name
    ON folders (account_id, parent_id, name);

CREATE INDEX IF NOT EXISTS threads_account_folder_date
    ON threads (account_id, folder_id, date DESC);
CREATE INDEX IF NOT EXISTS threads_account_unread
    ON threads (account_id, unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS threads_account_starred
    ON threads (account_id, starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS threads_snooze_until
    ON threads (snooze_until) WHERE snooze_until IS NOT NULL;

CREATE INDEX IF NOT EXISTS messages_account_thread
    ON messages (account_id, thread_id);
CREATE INDEX IF NOT EXISTS messages_account_provider_uid
    ON messages (account_id, provider_uid);
CREATE INDEX IF NOT EXISTS messages_message_id_header
    ON messages (message_id_header);
CREATE INDEX IF NOT EXISTS messages_account_folder_date
    ON messages (account_id, folder_id, date DESC);
CREATE INDEX IF NOT EXISTS messages_unread
    ON messages (unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS messages_starred
    ON messages (starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS messages_sender
    ON messages (account_id, sender_email);

CREATE INDEX IF NOT EXISTS snoozes_until ON snoozes (until_ts);
CREATE UNIQUE INDEX IF NOT EXISTS operations_idempotent
    ON operations (account_id, op, target_id, payload_hash)
    WHERE status IN ('pending', 'running');
CREATE INDEX IF NOT EXISTS search_history_created ON search_history (created_at DESC);
CREATE INDEX IF NOT EXISTS mcp_audit_created ON mcp_audit (created_at DESC);
CREATE INDEX IF NOT EXISTS outbox_account_status ON outbox (account_id, status);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5 (
    sender,
    recipients,
    subject,
    body,
    attachment_names,
    labels,
    message_id UNINDEXED,
    tokenize = 'unicode61'
);
"#;

    /// The real schema.sql this repo shipped at schema v7 -- i.e. before
    /// T-048 added `fts_pending` -- captured verbatim via `git show
    /// HEAD:crates/db/src/schema.sql` (run against the commit immediately
    /// before that change, not the working tree). Used by
    /// `migrate_upgrades_v7_profile_to_v8_backfilling_existing_messages_into_fts_pending`
    /// the same way `OLD_SCHEMA_V6_SQL` above is used for the v6->v7
    /// upgrade.
    const OLD_SCHEMA_V7_SQL: &str = r#"
-- Feather Mail schema v1 (T-005). D13 WAL + file cache, D14 no secrets, D15 indexes, D31 queue.

CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT NOT NULL,
    provider TEXT NOT NULL,
    imap_host TEXT,
    imap_port INTEGER,
    smtp_host TEXT,
    smtp_port INTEGER,
    imap_security TEXT,
    smtp_security TEXT,
    username TEXT,
    status TEXT NOT NULL DEFAULT 'offline',
    download_policy TEXT NOT NULL DEFAULT 'recent',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- T-079: folder identity is `remote_id` (the exact mailbox name the server's
-- `LIST` returned), not the display `name` -- two real mailboxes in
-- different branches (`Team/Ideas`, `Personal/Ideas`) can and do share a
-- display name. `remote_id` is NULL for a local-only folder not yet
-- reconciled with the server (`Core::create_folder`'s placeholder row,
-- T-077's adoption case); SQLite's UNIQUE allows any number of NULLs, so
-- multiple such rows coexisting is intended, not a gap. `name` is unique
-- only within one branch -- see `folders_account_parent_name` below, which
-- enforces that in `parent_id`'s stead since a plain column-level UNIQUE
-- can't express "unique per sibling group".
CREATE TABLE IF NOT EXISTS folders (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    remote_id TEXT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    parent_id TEXT REFERENCES folders(id),
    color TEXT,
    UNIQUE (account_id, remote_id)
);

CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id),
    subject TEXT NOT NULL,
    snippet TEXT NOT NULL DEFAULT '',
    date INTEGER NOT NULL,
    unread INTEGER NOT NULL DEFAULT 0,
    starred INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    deleted INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    importance INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 1,
    snooze_until INTEGER,
    gm_thrid TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id),
    provider_uid INTEGER,
    message_id_header TEXT,
    in_reply_to TEXT,
    references_header TEXT,
    date INTEGER NOT NULL,
    sender_name TEXT NOT NULL DEFAULT '',
    sender_email TEXT NOT NULL DEFAULT '',
    recipients TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    snippet TEXT NOT NULL DEFAULT '',
    unread INTEGER NOT NULL DEFAULT 0,
    starred INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    importance INTEGER NOT NULL DEFAULT 0,
    body_path TEXT,
    size_bytes INTEGER NOT NULL DEFAULT 0,
    -- T-024 (schema v4): size in bytes of the *cached body file* at
    -- body_path, so the cache-limit sweep can SUM() this column instead of
    -- stat()-ing every file on disk. Distinct from size_bytes above, which
    -- is the server-reported RFC822.SIZE and means something else -- do not
    -- conflate the two. NULL until a body is actually cached.
    body_bytes INTEGER
);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes INTEGER NOT NULL DEFAULT 0,
    cache_path TEXT,
    content_id TEXT
);

CREATE TABLE IF NOT EXISTS labels (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    color TEXT,
    UNIQUE (account_id, name)
);

CREATE TABLE IF NOT EXISTS message_labels (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    label_id TEXT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (message_id, label_id)
);

CREATE TABLE IF NOT EXISTS drafts (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT REFERENCES threads(id),
    in_reply_to TEXT,
    from_addr TEXT NOT NULL,
    to_addr TEXT NOT NULL DEFAULT '',
    cc TEXT NOT NULL DEFAULT '',
    bcc TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    remote_uid INTEGER
);

CREATE TABLE IF NOT EXISTS snoozes (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    until_ts INTEGER NOT NULL,
    previous_folder_id TEXT NOT NULL,
    UNIQUE (account_id, thread_id)
);

CREATE TABLE IF NOT EXISTS sync_state (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uidvalidity INTEGER,
    uidnext INTEGER,
    highest_modseq INTEGER,
    last_sync_at INTEGER,
    -- T-022 second half (schema v3): resumable newest-first backfill cursor.
    -- See feathermail_sync::FolderSyncState for the exact semantics.
    backfill_floor INTEGER,
    backfill_target INTEGER,
    -- T-078 (b) prep (schema v7): bookkeeping for
    -- feathermail_sync::schedule::next_sync's `FolderInput`, which is not
    -- the same axis as `last_sync_at` above. `last_attempt_at` is set on
    -- *every* sync attempt, successful or not; `last_sync_at` only moves on
    -- success. Without a separate attempt clock, a folder stuck in a
    -- failure loop never advances anything, so after a crash/restart the
    -- scheduler sees "never tried" and fires every folder at once instead
    -- of respecting backoff -- see `FolderInput::last_attempt_at`'s doc
    -- comment in crates/sync/src/schedule.rs for the full argument.
    -- `consecutive_failures` is what backoff is keyed on; it resets to 0 on
    -- success and increments on failure (`Core::record_sync_attempt`).
    -- Both must survive a restart (D32), hence columns, not in-memory state.
    last_attempt_at INTEGER,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, folder_id)
);

CREATE TABLE IF NOT EXISTS operations (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL,
    op TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    payload_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    status TEXT NOT NULL DEFAULT 'pending',
    -- T-081 (schema v6): the pre-mutation snapshot of the columns this
    -- operation's own command touched on `threads`, captured by
    -- `Core::dispatch` the instant before it wrote the optimistic local
    -- mark. Deliberately NOT folded into `payload`: `payload_hash` is D29's
    -- idempotency key, and mixing in the *previous* state would make two
    -- `Archive` calls on the same thread from different starting states
    -- hash to two different operations, breaking dedup. When a provider
    -- apply fails non-retryably (`ApplyError` other than `Network`, and
    -- other than `Conflict` which the queue treats as success),
    -- `feathermail_core::queue` reads this column and puts the mark back,
    -- so SQLite stops asserting a state the server never confirmed. NULL
    -- for operations that don't carry an undo (e.g. `create_folder`).
    undo_payload TEXT
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS search_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    query TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_clients (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_permissions (
    client_id TEXT NOT NULL REFERENCES mcp_clients(id) ON DELETE CASCADE,
    capability TEXT NOT NULL,
    grant TEXT NOT NULL,
    PRIMARY KEY (client_id, capability)
);

CREATE TABLE IF NOT EXISTS mcp_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id TEXT,
    tool TEXT NOT NULL,
    account_id TEXT,
    target_id TEXT,
    outcome TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    draft_id TEXT REFERENCES drafts(id),
    to_addr TEXT NOT NULL,
    subject TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued'
);

CREATE INDEX IF NOT EXISTS folders_account ON folders (account_id);
-- T-079: name uniqueness lives per sibling group, not per account, so two
-- real mailboxes under different parents (`Team/Ideas`, `Personal/Ideas`)
-- can both display as "Ideas". Deliberately a plain (not COALESCE'd)
-- multi-column index: SQLite treats `NULL != NULL` for UNIQUE purposes, so
-- two rows with `parent_id IS NULL` never collide on `name` here, same as
-- `remote_id IS NULL` never collides above. That is intentional, not an
-- oversight: `Core::sync_folders` links `parent_id` in a second pass after
-- every row in a `LIST` walk is written (see its doc comment), so a branch
-- whose own container folder was filtered out or simply hasn't been
-- discovered yet leaves its children's `parent_id` unset (top-level, by
-- this index's lights) -- e.g. three "Ideas" folders from three unrelated
-- branches, discovered without their parent containers, must still all
-- land as "Ideas" rather than fight over one root-level slot. Top-level
-- name collisions among folders a user actually creates locally are still
-- prevented -- one level up, in `Core::create_folder`'s own duplicate
-- check, not by this index.
CREATE UNIQUE INDEX IF NOT EXISTS folders_account_parent_name
    ON folders (account_id, parent_id, name);

CREATE INDEX IF NOT EXISTS threads_account_folder_date
    ON threads (account_id, folder_id, date DESC);
CREATE INDEX IF NOT EXISTS threads_account_unread
    ON threads (account_id, unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS threads_account_starred
    ON threads (account_id, starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS threads_snooze_until
    ON threads (snooze_until) WHERE snooze_until IS NOT NULL;

CREATE INDEX IF NOT EXISTS messages_account_thread
    ON messages (account_id, thread_id);
CREATE INDEX IF NOT EXISTS messages_account_provider_uid
    ON messages (account_id, provider_uid);
CREATE INDEX IF NOT EXISTS messages_message_id_header
    ON messages (message_id_header);
CREATE INDEX IF NOT EXISTS messages_account_folder_date
    ON messages (account_id, folder_id, date DESC);
CREATE INDEX IF NOT EXISTS messages_unread
    ON messages (unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS messages_starred
    ON messages (starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS messages_sender
    ON messages (account_id, sender_email);

CREATE INDEX IF NOT EXISTS snoozes_until ON snoozes (until_ts);
CREATE UNIQUE INDEX IF NOT EXISTS operations_idempotent
    ON operations (account_id, op, target_id, payload_hash)
    WHERE status IN ('pending', 'running');
CREATE INDEX IF NOT EXISTS search_history_created ON search_history (created_at DESC);
CREATE INDEX IF NOT EXISTS mcp_audit_created ON mcp_audit (created_at DESC);
CREATE INDEX IF NOT EXISTS outbox_account_status ON outbox (account_id, status);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5 (
    sender,
    recipients,
    subject,
    body,
    attachment_names,
    labels,
    message_id UNINDEXED,
    tokenize = 'unicode61'
);
"#;

    /// The real schema.sql this repo shipped at schema v8 -- i.e. after
    /// T-048 added `fts_pending` but before T-093 added the v8->v9
    /// backfill -- captured verbatim via `git show HEAD:crates/db/src/schema.sql`
    /// (run against the commit immediately before this change, not the
    /// working tree, per the same rule `OLD_SCHEMA_V7_SQL` above follows).
    /// Used by
    /// `migrate_upgrades_v8_profile_to_v9_backfilling_only_messages_with_a_cached_body`
    /// the same way `OLD_SCHEMA_V7_SQL` above is used for the v7->v8
    /// upgrade.
    const OLD_SCHEMA_V8_SQL: &str = r#"
-- Feather Mail schema v1 (T-005). D13 WAL + file cache, D14 no secrets, D15 indexes, D31 queue.

CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT NOT NULL,
    provider TEXT NOT NULL,
    imap_host TEXT,
    imap_port INTEGER,
    smtp_host TEXT,
    smtp_port INTEGER,
    imap_security TEXT,
    smtp_security TEXT,
    username TEXT,
    status TEXT NOT NULL DEFAULT 'offline',
    download_policy TEXT NOT NULL DEFAULT 'recent',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- T-079: folder identity is `remote_id` (the exact mailbox name the server's
-- `LIST` returned), not the display `name` -- two real mailboxes in
-- different branches (`Team/Ideas`, `Personal/Ideas`) can and do share a
-- display name. `remote_id` is NULL for a local-only folder not yet
-- reconciled with the server (`Core::create_folder`'s placeholder row,
-- T-077's adoption case); SQLite's UNIQUE allows any number of NULLs, so
-- multiple such rows coexisting is intended, not a gap. `name` is unique
-- only within one branch -- see `folders_account_parent_name` below, which
-- enforces that in `parent_id`'s stead since a plain column-level UNIQUE
-- can't express "unique per sibling group".
CREATE TABLE IF NOT EXISTS folders (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    remote_id TEXT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    parent_id TEXT REFERENCES folders(id),
    color TEXT,
    UNIQUE (account_id, remote_id)
);

CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id),
    subject TEXT NOT NULL,
    snippet TEXT NOT NULL DEFAULT '',
    date INTEGER NOT NULL,
    unread INTEGER NOT NULL DEFAULT 0,
    starred INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    deleted INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    importance INTEGER NOT NULL DEFAULT 0,
    message_count INTEGER NOT NULL DEFAULT 1,
    snooze_until INTEGER,
    gm_thrid TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id),
    provider_uid INTEGER,
    message_id_header TEXT,
    in_reply_to TEXT,
    references_header TEXT,
    date INTEGER NOT NULL,
    sender_name TEXT NOT NULL DEFAULT '',
    sender_email TEXT NOT NULL DEFAULT '',
    recipients TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    snippet TEXT NOT NULL DEFAULT '',
    unread INTEGER NOT NULL DEFAULT 0,
    starred INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    importance INTEGER NOT NULL DEFAULT 0,
    body_path TEXT,
    size_bytes INTEGER NOT NULL DEFAULT 0,
    -- T-024 (schema v4): size in bytes of the *cached body file* at
    -- body_path, so the cache-limit sweep can SUM() this column instead of
    -- stat()-ing every file on disk. Distinct from size_bytes above, which
    -- is the server-reported RFC822.SIZE and means something else -- do not
    -- conflate the two. NULL until a body is actually cached.
    body_bytes INTEGER
);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes INTEGER NOT NULL DEFAULT 0,
    cache_path TEXT,
    content_id TEXT
);

CREATE TABLE IF NOT EXISTS labels (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    color TEXT,
    UNIQUE (account_id, name)
);

CREATE TABLE IF NOT EXISTS message_labels (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    label_id TEXT NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (message_id, label_id)
);

CREATE TABLE IF NOT EXISTS drafts (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT REFERENCES threads(id),
    in_reply_to TEXT,
    from_addr TEXT NOT NULL,
    to_addr TEXT NOT NULL DEFAULT '',
    cc TEXT NOT NULL DEFAULT '',
    bcc TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL,
    remote_uid INTEGER
);

CREATE TABLE IF NOT EXISTS snoozes (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    until_ts INTEGER NOT NULL,
    previous_folder_id TEXT NOT NULL,
    UNIQUE (account_id, thread_id)
);

CREATE TABLE IF NOT EXISTS sync_state (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    folder_id TEXT NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uidvalidity INTEGER,
    uidnext INTEGER,
    highest_modseq INTEGER,
    last_sync_at INTEGER,
    -- T-022 second half (schema v3): resumable newest-first backfill cursor.
    -- See feathermail_sync::FolderSyncState for the exact semantics.
    backfill_floor INTEGER,
    backfill_target INTEGER,
    -- T-078 (b) prep (schema v7): bookkeeping for
    -- feathermail_sync::schedule::next_sync's `FolderInput`, which is not
    -- the same axis as `last_sync_at` above. `last_attempt_at` is set on
    -- *every* sync attempt, successful or not; `last_sync_at` only moves on
    -- success. Without a separate attempt clock, a folder stuck in a
    -- failure loop never advances anything, so after a crash/restart the
    -- scheduler sees "never tried" and fires every folder at once instead
    -- of respecting backoff -- see `FolderInput::last_attempt_at`'s doc
    -- comment in crates/sync/src/schedule.rs for the full argument.
    -- `consecutive_failures` is what backoff is keyed on; it resets to 0 on
    -- success and increments on failure (`Core::record_sync_attempt`).
    -- Both must survive a restart (D32), hence columns, not in-memory state.
    last_attempt_at INTEGER,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, folder_id)
);

CREATE TABLE IF NOT EXISTS operations (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_id TEXT NOT NULL,
    op TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    payload_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    status TEXT NOT NULL DEFAULT 'pending',
    -- T-081 (schema v6): the pre-mutation snapshot of the columns this
    -- operation's own command touched on `threads`, captured by
    -- `Core::dispatch` the instant before it wrote the optimistic local
    -- mark. Deliberately NOT folded into `payload`: `payload_hash` is D29's
    -- idempotency key, and mixing in the *previous* state would make two
    -- `Archive` calls on the same thread from different starting states
    -- hash to two different operations, breaking dedup. When a provider
    -- apply fails non-retryably (`ApplyError` other than `Network`, and
    -- other than `Conflict` which the queue treats as success),
    -- `feathermail_core::queue` reads this column and puts the mark back,
    -- so SQLite stops asserting a state the server never confirmed. NULL
    -- for operations that don't carry an undo (e.g. `create_folder`).
    undo_payload TEXT
);

CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS search_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    query TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_clients (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_permissions (
    client_id TEXT NOT NULL REFERENCES mcp_clients(id) ON DELETE CASCADE,
    capability TEXT NOT NULL,
    grant TEXT NOT NULL,
    PRIMARY KEY (client_id, capability)
);

CREATE TABLE IF NOT EXISTS mcp_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id TEXT,
    tool TEXT NOT NULL,
    account_id TEXT,
    target_id TEXT,
    outcome TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS outbox (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    draft_id TEXT REFERENCES drafts(id),
    to_addr TEXT NOT NULL,
    subject TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued'
);

CREATE INDEX IF NOT EXISTS folders_account ON folders (account_id);
-- T-079: name uniqueness lives per sibling group, not per account, so two
-- real mailboxes under different parents (`Team/Ideas`, `Personal/Ideas`)
-- can both display as "Ideas". Deliberately a plain (not COALESCE'd)
-- multi-column index: SQLite treats `NULL != NULL` for UNIQUE purposes, so
-- two rows with `parent_id IS NULL` never collide on `name` here, same as
-- `remote_id IS NULL` never collides above. That is intentional, not an
-- oversight: `Core::sync_folders` links `parent_id` in a second pass after
-- every row in a `LIST` walk is written (see its doc comment), so a branch
-- whose own container folder was filtered out or simply hasn't been
-- discovered yet leaves its children's `parent_id` unset (top-level, by
-- this index's lights) -- e.g. three "Ideas" folders from three unrelated
-- branches, discovered without their parent containers, must still all
-- land as "Ideas" rather than fight over one root-level slot. Top-level
-- name collisions among folders a user actually creates locally are still
-- prevented -- one level up, in `Core::create_folder`'s own duplicate
-- check, not by this index.
CREATE UNIQUE INDEX IF NOT EXISTS folders_account_parent_name
    ON folders (account_id, parent_id, name);

CREATE INDEX IF NOT EXISTS threads_account_folder_date
    ON threads (account_id, folder_id, date DESC);
CREATE INDEX IF NOT EXISTS threads_account_unread
    ON threads (account_id, unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS threads_account_starred
    ON threads (account_id, starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS threads_snooze_until
    ON threads (snooze_until) WHERE snooze_until IS NOT NULL;

CREATE INDEX IF NOT EXISTS messages_account_thread
    ON messages (account_id, thread_id);
CREATE INDEX IF NOT EXISTS messages_account_provider_uid
    ON messages (account_id, provider_uid);
CREATE INDEX IF NOT EXISTS messages_message_id_header
    ON messages (message_id_header);
CREATE INDEX IF NOT EXISTS messages_account_folder_date
    ON messages (account_id, folder_id, date DESC);
CREATE INDEX IF NOT EXISTS messages_unread
    ON messages (unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS messages_starred
    ON messages (starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS messages_sender
    ON messages (account_id, sender_email);

CREATE INDEX IF NOT EXISTS snoozes_until ON snoozes (until_ts);
CREATE UNIQUE INDEX IF NOT EXISTS operations_idempotent
    ON operations (account_id, op, target_id, payload_hash)
    WHERE status IN ('pending', 'running');
CREATE INDEX IF NOT EXISTS search_history_created ON search_history (created_at DESC);
CREATE INDEX IF NOT EXISTS mcp_audit_created ON mcp_audit (created_at DESC);
CREATE INDEX IF NOT EXISTS outbox_account_status ON outbox (account_id, status);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5 (
    sender,
    recipients,
    subject,
    body,
    attachment_names,
    labels,
    message_id UNINDEXED,
    tokenize = 'unicode61'
);

-- T-048 (schema v8): the queue that separates "a message row exists" from
-- "that message is searchable". A row here means `messages_fts` does not
-- yet reflect this message's *current* state -- either it has never been
-- indexed at all (just synced, body maybe not even downloaded yet), or it
-- was indexed once but a fact `messages_fts` depends on has since changed
-- (most importantly: the body was cached after the metadata-only insert
-- indexed it with an empty body, see `Core::store_body`). The background
-- indexer (`Core::index_pending_batch`) drains this table in `queued_at`
-- order; `Core::search`'s `pending_index` count is `COUNT(*)` on it, so a
-- caller can tell "no results" from "no results *yet*". `ON DELETE
-- CASCADE` means a message removed from `messages` (sync deletion,
-- `remove_account`, `reset_folder`) never leaves an orphaned queue entry
-- the indexer would try to look up and silently skip -- one less thing to
-- get wrong, since `messages_fts` itself has no FK to lean on (it is a
-- virtual table, see the comment above and `Core::remove_account`).
CREATE TABLE IF NOT EXISTS fts_pending (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    queued_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS fts_pending_queued_at ON fts_pending (queued_at);
"#;

    #[test]
    fn crate_compiles() {
        assert!(crate_name().starts_with("feathermail-"));
    }

    #[test]
    fn default_path_is_under_feathermail() {
        let path = default_db_path();
        assert_eq!(path.file_name().unwrap(), "mail.db");
        assert!(path.components().any(|c| c.as_os_str() == "feathermail"));
    }

    #[test]
    fn migrate_empty_file_creates_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let names: HashSet<_> = db.table_names().unwrap().into_iter().collect();
        for table in REQUIRED_TABLES {
            assert!(names.contains(*table), "missing table {table}");
        }
        assert!(names.contains("schema_migrations"));
        assert!(names.contains("messages_fts"));
        assert_eq!(db.journal_mode().unwrap(), "wal");
    }

    #[test]
    fn migrate_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let first = Database::open(&path).unwrap();
        let tables = first.table_names().unwrap();
        drop(first);
        let second = Database::open(&path).unwrap();
        assert_eq!(second.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(second.table_names().unwrap(), tables);
        let count: i32 = second
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn inbox_page_uses_account_folder_date_index() {
        let db = Database::memory().unwrap();
        let plan = db
            .explain_query_plan(INBOX_PAGE_SQL, &[&"john", &"inbox"])
            .unwrap();
        assert!(
            plan.contains("threads_account_folder_date"),
            "inbox page must use D15 index, plan was:\n{plan}"
        );
        let lower = plan.to_ascii_lowercase();
        assert!(
            !lower.contains("scan threads") || lower.contains("using index"),
            "large SELECT without index, plan was:\n{plan}"
        );
    }

    #[test]
    fn accounts_store_security_not_secrets() {
        let db = Database::memory().unwrap();
        let cols = db.column_names().unwrap();
        assert!(cols
            .iter()
            .any(|(t, c)| t == "accounts" && c == "imap_security"));
        assert!(cols
            .iter()
            .any(|(t, c)| t == "accounts" && c == "smtp_security"));
    }

    /// T-134: a profile that already synced mail under the broken
    /// `html_to_text` carries the ampersand runs in `messages.snippet`.
    /// The upgrade must queue exactly those rows -- not every message
    /// (that would re-read a whole mailbox off disk for nothing) and not
    /// the ones with no cached body to recompute from.
    #[test]
    fn migrate_queues_the_damaged_snippets_and_only_those() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let db = Database::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('acct', 'Reader', 'reader@example.test', 'imap', 0, 0);
                     INSERT INTO folders (id, account_id, name, kind)
                     VALUES ('acct:inbox', 'acct', 'Inbox', 'inbox');
                     INSERT INTO threads (id, account_id, folder_id, subject, snippet, date)
                     VALUES ('t1', 'acct', 'acct:inbox', 'Sale', '', 1);
                     INSERT INTO messages
                        (id, account_id, thread_id, folder_id, date, snippet, body_path)
                     VALUES
                        ('broken', 'acct', 't1', 'acct:inbox', 1, '&&&&&&&& Real text',
                         'ac/broken.eml'),
                        ('clean', 'acct', 't1', 'acct:inbox', 2, 'Real text', 'ac/clean.eml'),
                        ('nobody', 'acct', 't1', 'acct:inbox', 3, '&&&& no cached body', NULL);
                     DELETE FROM snippet_repairs;
                     DELETE FROM schema_migrations;
                     INSERT INTO schema_migrations (version, applied_at) VALUES (25, 0);",
                )
                .unwrap();
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let queued: Vec<String> = db
            .conn
            .prepare("SELECT message_id FROM snippet_repairs ORDER BY message_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            queued,
            vec!["broken".to_string()],
            "only a damaged snippet with a body to recompute from is queued"
        );
    }

    /// T-141: a profile made before the default changed carries the old
    /// default in its `settings` row -- the row is rewritten on every
    /// flush whether the reader touched the toggle or not, so "true" there
    /// is not a decision. The upgrade retires it with the default; keys
    /// this migration has no business in are left alone.
    #[test]
    fn migrate_retires_the_old_prefer_plain_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let db = Database::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES
                        ('prefer_plain', 'true'),
                        ('block_remote', 'true'),
                        ('theme', 'dark');
                     DELETE FROM schema_migrations;
                     INSERT INTO schema_migrations (version, applied_at) VALUES (26, 0);",
                )
                .unwrap();
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let read = |key: &str| -> String {
            db.conn
                .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        assert_eq!(read("prefer_plain"), "false", "the old default is retired");
        assert_eq!(
            read("block_remote"),
            "true",
            "remote images are a different toggle and stay blocked"
        );
        assert_eq!(read("theme"), "dark", "nothing else is touched");
    }

    #[test]
    fn migrate_upgrades_v13_profile_with_drafts_preserving_the_local_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
                 INSERT INTO schema_migrations (version, applied_at) VALUES (13, 0);
                 CREATE TABLE accounts (id TEXT PRIMARY KEY);
                 CREATE TABLE drafts (
                    id TEXT PRIMARY KEY,
                    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                    thread_id TEXT,
                    in_reply_to TEXT,
                    from_addr TEXT NOT NULL,
                    to_addr TEXT NOT NULL DEFAULT '',
                    cc TEXT NOT NULL DEFAULT '',
                    bcc TEXT NOT NULL DEFAULT '',
                    subject TEXT NOT NULL DEFAULT '',
                    body TEXT NOT NULL DEFAULT '',
                    updated_at INTEGER NOT NULL,
                    remote_uid INTEGER
                 );
                 INSERT INTO accounts (id) VALUES ('acct');
                 INSERT INTO drafts (id, account_id, from_addr, updated_at)
                 VALUES ('draft:acct:1', 'acct', 'writer@example.test', 17);",
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let revision: i64 = db
            .conn
            .query_row(
                "SELECT sync_revision FROM drafts WHERE id = 'draft:acct:1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revision, 0, "migration must retain existing local drafts");
    }

    #[test]
    fn migrate_upgrades_v14_attachment_rows_with_streaming_part_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
                 INSERT INTO schema_migrations (version, applied_at) VALUES (14, 1000);
                 CREATE TABLE attachments (
                    id TEXT PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    filename TEXT NOT NULL,
                    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
                    size_bytes INTEGER NOT NULL DEFAULT 0,
                    cache_path TEXT,
                    content_id TEXT
                 );
                 INSERT INTO attachments (id, account_id, message_id, filename)
                 VALUES ('a1', 'acc1', 'm1', 'old.pdf');",
            )
            .unwrap();
        }
        let db = Database::open(&path).unwrap();
        let columns: Vec<String> = db
            .conn()
            .prepare("PRAGMA table_info(attachments)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(columns.contains(&"part_path".to_string()));
        assert!(columns.contains(&"transfer_encoding".to_string()));
        let row: (Option<String>, String) = db
            .conn()
            .query_row(
                "SELECT part_path, transfer_encoding FROM attachments WHERE id = 'a1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, None);
        assert_eq!(row.1, "identity");
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    /// T-046: a real v15 profile gains a separate Cc field without touching
    /// its existing message rows.  Reply all needs this header distinct from
    /// `recipients`; an empty value is the honest fallback until the next
    /// header metadata sync refreshes legacy rows.
    #[test]
    fn migrate_upgrades_v15_messages_with_empty_cc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            // Use the actual current schema, subtracting only the one v16
            // column.  A tiny hand-written `messages` table cannot exercise
            // `Database::open`: schema.sql also creates its real indexes.
            let v15_schema =
                include_str!("schema.sql").replacen("    cc TEXT NOT NULL DEFAULT '',\n", "", 1);
            conn.execute_batch(&v15_schema).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (15, 1000)",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                   VALUES ('acct', 'Writer', 'writer@example.test', 'generic', 0, 0);
                 INSERT INTO folders (id, account_id, name, kind)
                   VALUES ('folder', 'acct', 'Inbox', 'inbox');
                 INSERT INTO threads (id, account_id, folder_id, subject, date)
                   VALUES ('thread', 'acct', 'folder', 'Subject', 0);
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date, recipients)
                   VALUES ('m1', 'acct', 'thread', 'folder', 0, 'to@example.test');",
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        let cc: String = db
            .conn()
            .query_row("SELECT cc FROM messages WHERE id = 'm1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(cc, "");
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    /// T-059: a real v16 profile has clients but no durable coarse policy.
    /// Upgrade it in place, retain the client, and prove the new opaque
    /// confirmation rows disappear with their account while global client
    /// grants deliberately remain available for the profile.
    #[test]
    fn migrate_upgrades_v16_mcp_policy_and_confirmation_cascade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            let mut v16_schema = include_str!("schema.sql").replacen(
                "    permission_level TEXT NOT NULL DEFAULT 'draft',\n",
                "",
                1,
            );
            let request_start = v16_schema
                .find("-- T-059: bounded cross-process approval hand-off")
                .expect("current schema must contain T-059 request table");
            let request_end = request_start
                + v16_schema[request_start..]
                    .find("\nCREATE TABLE IF NOT EXISTS mcp_audit")
                    .expect("request table must end before mcp_audit");
            v16_schema.replace_range(request_start..request_end, "");
            let indexes_start = v16_schema
                .find("CREATE INDEX IF NOT EXISTS mcp_confirmation_pending")
                .expect("current schema must contain T-059 indexes");
            let indexes_end = indexes_start
                + v16_schema[indexes_start..]
                    .find("CREATE INDEX IF NOT EXISTS outbox_account_status")
                    .expect("T-059 indexes must precede outbox index");
            v16_schema.replace_range(indexes_start..indexes_end, "");
            assert!(
                !v16_schema.contains("mcp_confirmation_requests"),
                "the upgrade fixture must be genuinely v16"
            );
            conn.execute_batch(&v16_schema).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (16, 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO mcp_clients (id, name, enabled, created_at)
                 VALUES ('stdio', 'Local stdio', 1, 1000)",
                [],
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        let level: String = db
            .conn()
            .query_row(
                "SELECT permission_level FROM mcp_clients WHERE id = 'stdio'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(level, "draft");
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        db.conn()
            .execute_batch(
                "INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('acct', 'Account', 'account@example.test', 'generic', 0, 0);
                 INSERT INTO mcp_permissions (client_id, capability, grant)
                     VALUES ('stdio', 'send_draft', 'allow');
                 INSERT INTO mcp_confirmation_requests
                     (client_id, capability, account_id, fingerprint, status, created_at, expires_at)
                     VALUES ('stdio', 'send_draft', 'acct', 'opaque-revision', 'pending', 1, 2);",
            )
            .unwrap();
        db.conn()
            .execute("DELETE FROM accounts WHERE id = 'acct'", [])
            .unwrap();
        let requests: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM mcp_confirmation_requests",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let grants: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM mcp_permissions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(requests, 0, "account removal cascades pending requests");
        assert_eq!(grants, 1, "MCP grants are global client policy");
    }

    /// T-060l: the only new persisted confirmation field is a safe scalar
    /// count.  Upgrade an actual v17 schema and retain its old opaque row;
    /// the default must truthfully describe the single-target request.
    #[test]
    fn migrate_upgrades_v17_confirmation_requests_with_safe_target_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            let v17_schema = include_str!("schema.sql").replacen(
                "    target_count INTEGER NOT NULL DEFAULT 1,\n",
                "",
                1,
            );
            conn.execute_batch(&v17_schema).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (17, 1000)",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO mcp_clients (id, name, enabled, permission_level, created_at)
                     VALUES ('stdio', 'Local stdio', 1, 'full', 0);
                 INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('acct', 'Account', 'account@example.test', 'generic', 0, 0);
                 INSERT INTO mcp_confirmation_requests
                     (client_id, capability, account_id, target_id, fingerprint, status, created_at, expires_at)
                     VALUES ('stdio', 'delete_message', 'acct', 'opaque-thread', 'opaque-fingerprint', 'pending', 1, 2);",
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        let (target_id, target_count): (Option<String>, i64) = db
            .conn()
            .query_row(
                "SELECT target_id, target_count FROM mcp_confirmation_requests",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(target_id.as_deref(), Some("opaque-thread"));
        assert_eq!(target_count, 1);
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    /// T-068: v19 replaces the O(N) FTS `message_id` delete with an ordinary
    /// message-id -> FTS rowid map. Upgrade an actual v18 profile that already
    /// has searchable mail: its live FTS row must retain exactly its original
    /// FTS rowid, and reopening must not duplicate the map.
    /// T-060s: a v19 profile that predates the headless sync door gains it
    /// on open, keeps its rows, and can take a request immediately -- the
    /// FK to `accounts` included, since a request naming a removed account
    /// is one the shell could never honour.
    #[test]
    fn migrate_upgrades_v19_profile_with_the_sync_request_door() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            let mut v19_schema = include_str!("schema.sql").to_string();
            let door_start = v19_schema
                .find("-- T-060s (schema v20)")
                .expect("current schema must contain the v20 sync-request door");
            v19_schema.truncate(door_start);
            assert!(
                !v19_schema.contains("sync_requests"),
                "the upgrade fixture must be genuinely v19"
            );
            conn.execute_batch(&v19_schema).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (19, 1000)",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('acc', 'Account', 'account@example.invalid', 'generic', 0, 0);",
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        db.conn()
            .execute(
                "INSERT INTO sync_requests (account_id, requested_at) VALUES ('acc', 7)",
                [],
            )
            .unwrap();
        let pending: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM sync_requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(pending, 1);
        db.conn()
            .execute("DELETE FROM accounts WHERE id = 'acc'", [])
            .unwrap();
        let orphaned: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM sync_requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(orphaned, 0, "a removed account leaves no request behind");
    }

    #[test]
    fn migrate_upgrades_v18_profile_with_fts_rowid_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let original_rowid;
        {
            let conn = Connection::open(&path).unwrap();
            let mut v18_schema = include_str!("schema.sql").to_string();
            let map_start = v18_schema
                .find("-- T-068: FTS5 keeps `message_id` UNINDEXED")
                .expect("current schema must contain the v19 FTS rowid map");
            let map_end = map_start
                + v18_schema[map_start..]
                    .find("\n\n-- T-048")
                    .expect("v19 FTS rowid map must precede the T-048 queue comment");
            v18_schema.replace_range(map_start..map_end, "");
            assert!(
                !v18_schema.contains("fts_message_rows"),
                "the upgrade fixture must be genuinely v18"
            );
            conn.execute_batch(&v18_schema).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (18, 1000)",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('acc', 'Account', 'account@example.invalid', 'generic', 0, 0);
                 INSERT INTO folders (id, account_id, name, kind)
                     VALUES ('acc:inbox', 'acc', 'Inbox', 'inbox');
                 INSERT INTO threads (id, account_id, folder_id, subject, date)
                     VALUES ('acc:t:1', 'acc', 'acc:inbox', 'Old indexed mail', 1);
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date, subject)
                     VALUES ('acc:m:1', 'acc', 'acc:t:1', 'acc:inbox', 1, 'Old indexed mail');
                 INSERT INTO messages_fts
                     (sender, recipients, subject, body, attachment_names, labels, message_id)
                     VALUES ('Sender', '', 'Old indexed mail', '', '', '', 'acc:m:1');",
            )
            .unwrap();
            original_rowid = conn.last_insert_rowid();
        }

        let db = Database::open(&path).unwrap();
        let mapped_rowid: i64 = db
            .conn()
            .query_row(
                "SELECT fts_rowid FROM fts_message_rows WHERE message_id = 'acc:m:1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mapped_rowid, original_rowid);
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        drop(db);

        let reopened = Database::open(&path).unwrap();
        let maps: i64 = reopened
            .conn()
            .query_row("SELECT COUNT(*) FROM fts_message_rows", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(maps, 1, "reopening must not duplicate the FTS rowid map");
    }

    /// A crash can happen after v19 writes some map rows but before it marks
    /// the schema version. The next open must resume rather than reject the
    /// already-copied primary key as a duplicate.
    #[test]
    fn migrate_v19_resumes_after_a_partially_copied_fts_rowid_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(include_str!("schema.sql")).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (18, 0)",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('acc', 'Account', 'account@example.invalid', 'generic', 0, 0);
                 INSERT INTO folders (id, account_id, name, kind)
                     VALUES ('acc:inbox', 'acc', 'Inbox', 'inbox');
                 INSERT INTO threads (id, account_id, folder_id, subject, date)
                     VALUES ('acc:t:1', 'acc', 'acc:inbox', 'First indexed mail', 1),
                            ('acc:t:2', 'acc', 'acc:inbox', 'Second indexed mail', 2);
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date, subject)
                     VALUES ('acc:m:1', 'acc', 'acc:t:1', 'acc:inbox', 1, 'First indexed mail'),
                            ('acc:m:2', 'acc', 'acc:t:2', 'acc:inbox', 2, 'Second indexed mail');",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages_fts
                 (sender, recipients, subject, body, attachment_names, labels, message_id)
                 VALUES ('Sender', '', 'First indexed mail', '', '', '', 'acc:m:1')",
                [],
            )
            .unwrap();
            let first_rowid = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO messages_fts
                 (sender, recipients, subject, body, attachment_names, labels, message_id)
                 VALUES ('Sender', '', 'Second indexed mail', '', '', '', 'acc:m:2')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO fts_message_rows (message_id, fts_rowid) VALUES ('acc:m:1', ?1)",
                [first_rowid],
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let maps: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM fts_message_rows", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            maps, 2,
            "the partial map row must be retained and the missing row must resume"
        );
    }

    #[test]
    fn schema_has_no_secret_columns() {
        let db = Database::memory().unwrap();
        let cols = db.column_names().unwrap();
        for (table, col) in &cols {
            let name = col.to_ascii_lowercase();
            for banned in FORBIDDEN_COLUMNS {
                assert_ne!(name.as_str(), *banned, "secret column {table}.{col} (D14)");
            }
        }
        assert!(
            !cols.iter().any(|(t, c)| t == "attachments" && c == "blob"),
            "attachments must be files, not BLOB (D13)"
        );
        assert!(
            !cols
                .iter()
                .any(|(t, c)| t == "messages" && (c == "body" || c == "body_html")),
            "message bodies live on disk as body_path"
        );
        assert!(
            !cols
                .iter()
                .any(|(t, c)| t == "mcp_audit" && c.contains("body")),
            "mcp_audit must not store bodies (D61)"
        );
        assert!(
            !cols
                .iter()
                .any(|(t, c)| t == "mcp_confirmation_requests" && c.contains("body")),
            "confirmation hand-off must not store mail bodies"
        );
    }

    #[cfg(unix)]
    #[test]
    fn db_file_is_owner_rw_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let _db = Database::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// Thread rollup and thread retargeting both ask `messages` by
    /// `thread_id` alone, with no `account_id` to lead with, so
    /// `messages_account_thread` cannot serve them. Without an index whose
    /// leading column is `thread_id` every one of those becomes a full-table
    /// scan, and `CoreSyncStore::rollup_folder` runs seven of them *per
    /// thread* on every header batch: measured on a 205 909-message profile,
    /// one 200-header batch cost 71.0 s of pure CPU without this index and
    /// 58 ms with it. The cost scales with the mailbox, not the batch, which
    /// is exactly why a small test profile cannot notice it -- so the plan is
    /// what gets asserted here, not a duration.
    /// T-136: the merged view's sidebar counts must not scan `threads`.
    /// On the owner's profile that scan is 261 818 rows and about 1.8 s
    /// per recount, and it runs on the same handle a click on a letter
    /// waits for. A small test profile cannot feel that, so the plan is
    /// what gets asserted, not a duration -- same reasoning as the
    /// message-by-thread test below.
    #[test]
    fn the_merged_views_folder_counts_use_a_covering_index_not_a_scan() {
        let db = Database::memory().unwrap();
        for filter in [
            "t.folder_id IN (SELECT id FROM folders WHERE kind = 'inbox') \
             AND t.archived = 0 AND t.deleted = 0 AND t.snooze_until IS NULL",
            "t.folder_id IN (SELECT id FROM folders WHERE kind = 'sent') \
             AND t.archived = 0 AND t.deleted = 0 AND t.snooze_until IS NULL",
        ] {
            let sql = format!(
                "SELECT COUNT(*), COALESCE(SUM(t.unread), 0) FROM threads t WHERE {filter}"
            );
            let plan = db.explain_query_plan(&sql, &[]).unwrap();
            assert!(
                plan.contains("COVERING INDEX threads_folder_flags"),
                "the unified count must be a covering-index seek, not a table scan -- \
                 plan was:\n{plan}"
            );
            assert!(!plan.contains("SCAN t"), "still scanning threads:\n{plan}");
        }
    }

    #[test]
    fn messages_are_reachable_by_thread_id_alone_without_a_table_scan() {
        let db = Database::memory().unwrap();
        for sql in [
            "SELECT COUNT(*) FROM messages m WHERE m.thread_id = 't'",
            "SELECT m.date FROM messages m WHERE m.thread_id = 't' \
             ORDER BY m.date DESC, m.id DESC LIMIT 1",
            "UPDATE messages SET thread_id = 'a' WHERE thread_id = 'b'",
        ] {
            let mut stmt = db
                .conn()
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let plan: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(
                plan.iter()
                    .any(|step| step.contains("USING INDEX")
                        || step.contains("USING COVERING INDEX")),
                "no index serves `{sql}`; the plan was {plan:?}"
            );
            assert!(
                !plan.iter().any(|step| step.starts_with("SCAN messages")),
                "`{sql}` scans the whole messages table; the plan was {plan:?}"
            );
        }
    }

    #[test]
    fn tables_with_account_id_matches_known_schema() {
        // Documents today's `account_id`-column sweep list so a diff that
        // changes it is visible in review. This is NOT the exhaustiveness
        // guarantee for T-021 — a table can be account-scoped without an
        // `account_id` column (see `messages_fts`, keyed by message_id) and
        // this list would happily miss it. The real guarantee is
        // `every_table_is_accounted_for_by_remove_account_or_an_explicit_reason`
        // below, which walks every table SQLite has, not just this method's
        // output.
        let db = Database::memory().unwrap();
        let mut got = db.tables_with_account_id().unwrap();
        got.sort();
        let mut want = vec![
            "attachments",
            "draft_attachments",
            "drafts",
            "folders",
            "labels",
            "mcp_audit",
            "mcp_confirmation_requests",
            "messages",
            "operations",
            "outbox",
            "snoozes",
            "sync_requests",
            "sync_state",
            "threads",
        ];
        want.sort();
        assert_eq!(got, want);
        assert!(!got.iter().any(|t| t == "accounts"));
    }

    #[test]
    fn every_table_is_accounted_for_by_remove_account_or_an_explicit_reason() {
        // T-021 follow-up: `messages_fts` shipped with no `account_id`
        // column, so `tables_with_account_id()` never returned it and
        // `Core::remove_account` silently skipped it until this was caught
        // in review. That guard test only re-lists `tables_with_account_id`'s
        // own output, so it can't catch this class of gap by construction.
        //
        // This test instead walks *every* table SQLite actually has and
        // requires each one to be sorted into a category on purpose:
        // scoped by an `account_id` column, explicitly not account-scoped
        // at all, or swept some other way that `Core::remove_account`
        // documents. A brand-new table — a second FTS index, a cache
        // table, anything — fails this test until someone consciously
        // classifies it, instead of leaking a removed account's data
        // forever like `messages_fts` almost did.
        const NOT_ACCOUNT_SCOPED: &[&str] = &[
            "settings",
            "search_history",
            "mcp_clients",
            "mcp_permissions",
            "schema_migrations",
        ];
        // No `account_id` column of its own; account-scoped rows disappear
        // as a side effect of something `Core::remove_account` already does
        // to another table.
        const SWEPT_INDIRECTLY: &[(&str, &str)] = &[
            // Both FKs are `ON DELETE CASCADE` (schema.sql); rows cascade
            // away when `remove_account` deletes the owning messages/labels
            // rows under `PRAGMA defer_foreign_keys = ON`.
            ("message_labels", "cascades from messages/labels FKs"),
            // fts5 virtual table keyed by message_id, not account_id.
            // Core::remove_account issues an explicit rowid-mapped DELETE
            // before it deletes the account's messages.
            ("messages_fts", "swept explicitly in Core::remove_account"),
            // T-068: a message-id -> FTS rowid lookup table. Its FK makes
            // it disappear when the account's messages are removed; the
            // FTS virtual rows themselves are still explicitly removed
            // first by Core::remove_account.
            ("fts_message_rows", "cascades from the messages FK"),
            // fts5 shadow tables: SQLite maintains these itself whenever
            // `messages_fts` (above) is written to; never DELETE them
            // directly.
            ("messages_fts_data", "fts5 shadow table of messages_fts"),
            ("messages_fts_idx", "fts5 shadow table of messages_fts"),
            ("messages_fts_docsize", "fts5 shadow table of messages_fts"),
            ("messages_fts_config", "fts5 shadow table of messages_fts"),
            ("messages_fts_content", "fts5 shadow table of messages_fts"),
            // T-048: `fts_pending.message_id` is `REFERENCES messages(id)
            // ON DELETE CASCADE`, so its rows for a removed account's
            // messages disappear as a side effect of the `messages` delete
            // `remove_account`'s table-by-table sweep already does (that
            // delete runs under `PRAGMA defer_foreign_keys = ON`, same as
            // `message_labels` above) -- no separate statement needed.
            ("fts_pending", "cascades from the messages FK"),
            // T-134: same shape and same FK as `fts_pending` above -- a
            // queue keyed by message id, whose rows for a removed account
            // go with that account's `messages` rows.
            ("snippet_repairs", "cascades from the messages FK"),
            // T-076: operation_moves is scoped by its operation/message
            // foreign keys; removing either the account's operations or its
            // messages cascades these intent rows.
            ("operation_moves", "cascades from operations/messages FKs"),
            (
                "operation_move_history",
                "swept explicitly with the account's operations",
            ),
            ("outbox_attachments", "cascades from the owning outbox row"),
        ];

        let db = Database::memory().unwrap();
        let scoped = db.tables_with_account_id().unwrap();
        let indirectly: Vec<&str> = SWEPT_INDIRECTLY.iter().map(|(t, _)| *t).collect();
        let mut unaccounted = Vec::new();
        for table in db.table_names().unwrap() {
            if table == "accounts" {
                continue;
            }
            if scoped.contains(&table) {
                continue;
            }
            if NOT_ACCOUNT_SCOPED.contains(&table.as_str()) {
                continue;
            }
            if indirectly.contains(&table.as_str()) {
                continue;
            }
            unaccounted.push(table);
        }
        assert!(
            unaccounted.is_empty(),
            "table(s) {unaccounted:?} are not in tables_with_account_id(), not in \
             NOT_ACCOUNT_SCOPED, and not in SWEPT_INDIRECTLY -- classify this table \
             (does Core::remove_account need to sweep it?) before this test passes"
        );
    }

    /// T-022 second half: a profile last opened under schema v2 (no
    /// `backfill_floor`/`backfill_target` on `sync_state`) must upgrade to
    /// v3 in place -- gaining the two new columns without losing the
    /// `sync_state` row (or any other data) that was already there. Builds
    /// a v2-shaped database by hand (not via `schema.sql`, which now already
    /// declares the v3 columns) so this genuinely exercises the ALTER TABLE
    /// path a real pre-existing profile would hit, not just a fresh create.
    #[test]
    fn migrate_upgrades_v2_profile_to_v3_preserving_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
                 CREATE TABLE accounts (
                     id TEXT PRIMARY KEY, name TEXT NOT NULL, email TEXT NOT NULL,
                     provider TEXT NOT NULL, imap_security TEXT, smtp_security TEXT,
                     status TEXT NOT NULL DEFAULT 'offline',
                     download_policy TEXT NOT NULL DEFAULT 'recent',
                     created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
                 );
                 CREATE TABLE folders (
                     id TEXT PRIMARY KEY, account_id TEXT NOT NULL, remote_id TEXT,
                     name TEXT NOT NULL, kind TEXT NOT NULL, parent_id TEXT, color TEXT
                 );
                 CREATE TABLE sync_state (
                     account_id TEXT NOT NULL,
                     folder_id TEXT NOT NULL,
                     uidvalidity INTEGER,
                     uidnext INTEGER,
                     highest_modseq INTEGER,
                     last_sync_at INTEGER,
                     PRIMARY KEY (account_id, folder_id)
                 );
                 INSERT INTO schema_migrations (version, applied_at) VALUES (2, 1000);
                 INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('john', 'John Doe', 'john@example.com', 'generic', 1000, 1000);
                 INSERT INTO folders (id, account_id, name, kind) VALUES ('inbox', 'john', 'Inbox', 'inbox');
                 INSERT INTO sync_state (account_id, folder_id, uidvalidity, uidnext, highest_modseq, last_sync_at)
                     VALUES ('john', 'inbox', 111, 222, 333, 444);",
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        // A v2 profile jumps straight to the current SCHEMA_VERSION (4) --
        // migrate() only ever writes one schema_migrations row, at whatever
        // version is current, not one row per version crossed. This test
        // still exercises the v2->v3 ALTER TABLE path specifically (the
        // columns it asserts below), it just no longer stops at "3".
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let cols = db.column_names().unwrap();
        assert!(cols
            .iter()
            .any(|(t, c)| t == "sync_state" && c == "backfill_floor"));
        assert!(cols
            .iter()
            .any(|(t, c)| t == "sync_state" && c == "backfill_target"));

        // Pre-existing row survived the upgrade untouched, new columns NULL.
        let (uidvalidity, uidnext, highest_modseq, last_sync_at, floor, target): (
            i64,
            i64,
            i64,
            i64,
            Option<i64>,
            Option<i64>,
        ) = db
            .conn
            .query_row(
                "SELECT uidvalidity, uidnext, highest_modseq, last_sync_at, backfill_floor, backfill_target
                 FROM sync_state WHERE account_id = 'john' AND folder_id = 'inbox'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(
            (uidvalidity, uidnext, highest_modseq, last_sync_at),
            (111, 222, 333, 444)
        );
        assert_eq!(floor, None);
        assert_eq!(target, None);

        let migrations_after_first_open: i32 = db
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            migrations_after_first_open, 2,
            "v2 row plus one new row at the current SCHEMA_VERSION"
        );
        drop(db);

        // Reopening an already-current profile must be idempotent: no
        // duplicate migration row, no error re-adding columns that already
        // exist.
        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.schema_version().unwrap(), SCHEMA_VERSION);
        let migrations_after_second_open: i32 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_second_open, 2);
        let still_there: i64 = db2
            .conn
            .query_row(
                "SELECT uidvalidity FROM sync_state WHERE account_id = 'john' AND folder_id = 'inbox'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_there, 111);
    }

    /// T-024: a profile last opened under schema v3 (no `messages.body_bytes`)
    /// must upgrade to v4 in place -- gaining the new column without losing
    /// the `messages` row (including a populated `body_path`) that was
    /// already there. Builds a v3-shaped database by hand (not via
    /// `schema.sql`, which now already declares the v4 column) so this
    /// genuinely exercises the ALTER TABLE path a real pre-existing profile
    /// would hit, not just a fresh create.
    #[test]
    fn migrate_upgrades_v3_profile_to_v4_preserving_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
                 CREATE TABLE accounts (
                     id TEXT PRIMARY KEY, name TEXT NOT NULL, email TEXT NOT NULL,
                     provider TEXT NOT NULL,
                     status TEXT NOT NULL DEFAULT 'offline',
                     download_policy TEXT NOT NULL DEFAULT 'recent',
                     created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
                 );
                 CREATE TABLE folders (
                     id TEXT PRIMARY KEY, account_id TEXT NOT NULL, remote_id TEXT,
                     name TEXT NOT NULL, kind TEXT NOT NULL, parent_id TEXT, color TEXT
                 );
                 CREATE TABLE threads (
                     id TEXT PRIMARY KEY,
                     account_id TEXT NOT NULL,
                     folder_id TEXT NOT NULL,
                     subject TEXT NOT NULL,
                     snippet TEXT NOT NULL DEFAULT '',
                     date INTEGER NOT NULL,
                     unread INTEGER NOT NULL DEFAULT 0,
                     starred INTEGER NOT NULL DEFAULT 0,
                     archived INTEGER NOT NULL DEFAULT 0,
                     deleted INTEGER NOT NULL DEFAULT 0,
                     has_attachment INTEGER NOT NULL DEFAULT 0,
                     importance INTEGER NOT NULL DEFAULT 0,
                     message_count INTEGER NOT NULL DEFAULT 1,
                     snooze_until INTEGER,
                     gm_thrid TEXT
                 );
                 CREATE TABLE messages (
                     id TEXT PRIMARY KEY,
                     account_id TEXT NOT NULL,
                     thread_id TEXT NOT NULL,
                     folder_id TEXT NOT NULL,
                     provider_uid INTEGER,
                     message_id_header TEXT,
                     in_reply_to TEXT,
                     references_header TEXT,
                     date INTEGER NOT NULL,
                     sender_name TEXT NOT NULL DEFAULT '',
                     sender_email TEXT NOT NULL DEFAULT '',
                     recipients TEXT NOT NULL DEFAULT '',
                     subject TEXT NOT NULL DEFAULT '',
                     snippet TEXT NOT NULL DEFAULT '',
                     unread INTEGER NOT NULL DEFAULT 0,
                     starred INTEGER NOT NULL DEFAULT 0,
                     has_attachment INTEGER NOT NULL DEFAULT 0,
                     importance INTEGER NOT NULL DEFAULT 0,
                     body_path TEXT,
                     size_bytes INTEGER NOT NULL DEFAULT 0
                 );
                 INSERT INTO schema_migrations (version, applied_at) VALUES (3, 1000);
                 INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('john', 'John Doe', 'john@example.com', 'generic', 1000, 1000);
                 INSERT INTO folders (id, account_id, name, kind) VALUES ('inbox', 'john', 'Inbox', 'inbox');
                 INSERT INTO threads (id, account_id, folder_id, subject, date)
                     VALUES ('t1', 'john', 'inbox', 'hi', 1000);
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date, body_path, size_bytes)
                     VALUES ('m1', 'john', 't1', 'inbox', 1000, 'bodies/ab/m1.body', 4096);",
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        // A v3 profile jumps straight to the current SCHEMA_VERSION (5, not
        // 4) -- migrate() only ever writes one schema_migrations row, at
        // whatever version is current, same as the v2->v3 test above. This
        // still exercises the v3->v4 ALTER TABLE path specifically (the
        // column it asserts below).
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let cols = db.column_names().unwrap();
        assert!(cols
            .iter()
            .any(|(t, c)| t == "messages" && c == "body_bytes"));

        // Pre-existing row survived the upgrade untouched, new column NULL.
        let (body_path, size_bytes, body_bytes): (String, i64, Option<i64>) = db
            .conn
            .query_row(
                "SELECT body_path, size_bytes, body_bytes FROM messages WHERE id = 'm1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(body_path, "bodies/ab/m1.body");
        assert_eq!(size_bytes, 4096);
        assert_eq!(body_bytes, None);

        let migrations_after_first_open: i32 = db
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            migrations_after_first_open, 2,
            "v3 row plus one new row at the current SCHEMA_VERSION"
        );
        drop(db);

        // Reopening an already-current profile must be idempotent.
        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.schema_version().unwrap(), SCHEMA_VERSION);
        let migrations_after_second_open: i32 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_second_open, 2);
        let still_there: String = db2
            .conn
            .query_row("SELECT body_path FROM messages WHERE id = 'm1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(still_there, "bodies/ab/m1.body");
    }

    /// T-079: a real pre-T-079 profile (schema v4, `folders` still carrying
    /// `UNIQUE (account_id, name)`) upgrades to v5 in place -- the
    /// constraint changes to `UNIQUE (account_id, remote_id)` plus the
    /// `parent_id`-scoped `folders_account_parent_name` index -- without
    /// losing a single row, including the FK chain `threads.folder_id` /
    /// `messages.folder_id` -> `folders.id` that the rebuild's `DROP TABLE
    /// folders` briefly makes dangling (see `rebuild_folders_table_for_v5`'s
    /// doc comment for why that's safe under `foreign_keys = ON`).
    #[test]
    fn migrate_upgrades_v4_profile_to_v5_rebuilding_folders_preserving_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(OLD_SCHEMA_V4_SQL).unwrap();
            conn.execute_batch(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (4, 1000);
                 INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('john', 'John Doe', 'john@example.com', 'generic', 1000, 1000);
                 INSERT INTO folders (id, account_id, remote_id, name, kind)
                     VALUES ('inbox', 'john', 'INBOX', 'Inbox', 'inbox');
                 INSERT INTO threads (id, account_id, folder_id, subject, snippet, date)
                     VALUES ('t1', 'john', 'inbox', 'Hi', 'Hi', 1000);
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date, body_path, size_bytes)
                     VALUES ('m1', 'john', 't1', 'inbox', 1000, 'bodies/ab/m1.body', 4096);",
            )
            .unwrap();

            // Sanity: this really is the old constraint on the real v4
            // schema text, not a fixture that happens to already match v5.
            let folders_sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name = 'folders'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(folders_sql.contains("UNIQUE (account_id, name)"));
        }

        let db = Database::open(&path).unwrap();
        // A v4 profile jumps straight to the current SCHEMA_VERSION (6, not
        // 5) -- migrate() only ever writes one schema_migrations row, at
        // whatever version is current, same as the v2->v3 and v3->v4 tests
        // above. This still exercises the v4->v5 `folders` rebuild path
        // specifically (the constraint asserted below).
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        // The constraint actually changed.
        let folders_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'folders'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            folders_sql.contains("UNIQUE (account_id, remote_id)"),
            "folders schema after migration was:\n{folders_sql}"
        );
        assert!(
            !folders_sql.contains("UNIQUE (account_id, name)"),
            "old constraint must be gone, folders schema was:\n{folders_sql}"
        );
        let parent_name_index_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'folders_account_parent_name'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(parent_name_index_sql.contains("parent_id"));
        assert!(parent_name_index_sql.contains("name"));

        // Every row survived, including the FK link from threads/messages
        // to folders -- the rebuild's DROP TABLE folders step is exactly
        // where that link could have been silently orphaned.
        let (account_id, email): (String, String) = db
            .conn
            .query_row(
                "SELECT id, email FROM accounts WHERE id = 'john'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(account_id, "john");
        assert_eq!(email, "john@example.com");

        let (folder_remote_id, folder_name): (Option<String>, String) = db
            .conn
            .query_row(
                "SELECT remote_id, name FROM folders WHERE id = 'inbox'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(folder_remote_id.as_deref(), Some("INBOX"));
        assert_eq!(folder_name, "Inbox");

        let thread_folder: String = db
            .conn
            .query_row("SELECT folder_id FROM threads WHERE id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            thread_folder, "inbox",
            "threads.folder_id -> folders.id survived the rebuild"
        );

        let message_folder: String = db
            .conn
            .query_row("SELECT folder_id FROM messages WHERE id = 'm1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(message_folder, "inbox");

        let migrations_after_first_open: i32 = db
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            migrations_after_first_open, 2,
            "v4 row plus one new row at the current SCHEMA_VERSION"
        );
        drop(db);

        // Reopening an already-current profile is idempotent: no duplicate
        // migration row, no error re-running the rebuild.
        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.schema_version().unwrap(), SCHEMA_VERSION);
        let migrations_after_second_open: i32 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_second_open, 2);
        let still_there: String = db2
            .conn
            .query_row("SELECT folder_id FROM threads WHERE id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(still_there, "inbox");
    }

    /// T-081: a real v5 profile (built from the actual historical
    /// `OLD_SCHEMA_V5_SQL`, not a hand-reduced fixture) gets
    /// `operations.undo_payload` added and jumps straight to
    /// `SCHEMA_VERSION` (6), with every existing row -- including one
    /// already sitting in `operations` before the column existed --
    /// preserved.
    #[test]
    fn migrate_upgrades_v5_profile_to_v6_adding_undo_payload_preserving_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(OLD_SCHEMA_V5_SQL).unwrap();
            conn.execute_batch(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (5, 1000);
                 INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('john', 'John Doe', 'john@example.com', 'generic', 1000, 1000);
                 INSERT INTO folders (id, account_id, remote_id, name, kind)
                     VALUES ('inbox', 'john', 'INBOX', 'Inbox', 'inbox');
                 INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
                     VALUES ('t1', 'john', 'inbox', 'Hi', 'Hi', 1000, 1);
                 INSERT INTO operations
                     (id, account_id, target_id, op, payload, payload_hash, created_at, retry_count, status)
                     VALUES ('archive:john:t1:aaaa', 'john', 't1', 'archive', '{}', 'aaaa', 1000, 0, 'failed');",
            )
            .unwrap();

            // Sanity: this really is the old shape -- no `undo_payload`
            // column yet -- not a fixture that happens to already match v6.
            let mut stmt = conn.prepare("PRAGMA table_info(operations)").unwrap();
            let has_undo = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .filter_map(std::result::Result::ok)
                .any(|name| name == "undo_payload");
            assert!(!has_undo, "fixture must predate undo_payload");
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        let mut stmt = db.conn.prepare("PRAGMA table_info(operations)").unwrap();
        let has_undo = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|name| name == "undo_payload");
        drop(stmt);
        assert!(
            has_undo,
            "operations.undo_payload must exist after migrating to v6"
        );

        // The pre-existing operations row survived, and its new column
        // defaults to NULL rather than erroring or losing the row.
        let (status, undo_payload): (String, Option<String>) = db
            .conn
            .query_row(
                "SELECT status, undo_payload FROM operations WHERE id = 'archive:john:t1:aaaa'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(undo_payload, None);

        let thread_unread: i64 = db
            .conn
            .query_row("SELECT unread FROM threads WHERE id = 't1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(thread_unread, 1);

        let migrations_after_first_open: i32 = db
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_first_open, 2, "v5 row plus the new v6 row");
        drop(db);

        // Reopening an already-v6 profile is idempotent: no duplicate
        // migration row, and (critically, per T-079's lesson) no "duplicate
        // column name" error from re-running `ADD COLUMN` on a column that
        // is now already there.
        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.schema_version().unwrap(), SCHEMA_VERSION);
        let migrations_after_second_open: i32 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_second_open, 2);
    }

    /// The other half of T-079's lesson, applied to T-081: a database
    /// created fresh from `schema.sql` already has `undo_payload` (the
    /// `current < 6` block's `add_column_if_missing` must see that and do
    /// nothing) -- checked here by opening a *brand new* profile and
    /// confirming both that the column exists and that `migrate()` did not
    /// error doing it, which a bare `ALTER TABLE ... ADD COLUMN` would have.
    #[test]
    fn a_fresh_profile_already_has_undo_payload() {
        let db = Database::memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let mut stmt = db.conn.prepare("PRAGMA table_info(operations)").unwrap();
        let has_undo = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .any(|name| name == "undo_payload");
        assert!(has_undo);
    }

    /// The gate against `schema.sql` and `migrate()` drifting apart.
    ///
    /// `migrate()` runs `schema.sql` first and only then the numbered
    /// blocks, and `schema.sql` is all `CREATE TABLE IF NOT EXISTS`. On a
    /// profile that already has the table, that statement does nothing at
    /// all -- so a column added to `schema.sql` but forgotten in a
    /// `migrate()` block exists on every new install and on none of the
    /// upgraded ones. Nothing before this test noticed: each migration
    /// test only asserts about the columns *it* introduced, and
    /// `add_column_if_missing` quietly heals the fresh-profile side of
    /// exactly the mistake we want to catch (revert a column out of
    /// `schema.sql` and the v7 block puts it straight back).
    ///
    /// So compare the two profiles that must agree and never do so by
    /// construction: one built from scratch by today's `schema.sql`, one
    /// built from the oldest historical schema this crate still carries a
    /// fixture for and then migrated all the way up.
    ///
    /// Compared as a set per table, not as an ordered list: `ALTER TABLE`
    /// can only append, while `schema.sql` may declare the same column
    /// anywhere, so the orders legitimately differ and only the shapes
    /// have to match. That is the weaker of the two possible bars, and it
    /// is the honest one -- a column-order difference is invisible to
    /// every query in this workspace, all of which name their columns.
    #[test]
    fn a_migrated_profile_ends_up_with_the_same_schema_as_a_fresh_one() {
        fn shape(conn: &Connection) -> Vec<(String, String)> {
            let mut objects: Vec<(String, String)> = conn
                .prepare(
                    "SELECT type || ' ' || name, COALESCE(tbl_name, '') FROM sqlite_master \
                     WHERE name NOT LIKE 'sqlite_%' ORDER BY 1",
                )
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .filter_map(std::result::Result::ok)
                .collect();

            let tables: Vec<String> = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' \
                     AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .filter_map(std::result::Result::ok)
                .collect();
            for table in tables {
                let mut cols: Vec<String> = conn
                    .prepare(&format!("PRAGMA table_info({table})"))
                    .unwrap()
                    .query_map([], |row| {
                        let name: String = row.get(1)?;
                        let decl: String = row.get(2)?;
                        let notnull: i64 = row.get(3)?;
                        let dflt: Option<String> = row.get(4)?;
                        let pk: i64 = row.get(5)?;
                        Ok(format!(
                            "{name} {decl} notnull={notnull} default={} pk={pk}",
                            dflt.unwrap_or_else(|| "-".into())
                        ))
                    })
                    .unwrap()
                    .filter_map(std::result::Result::ok)
                    .collect();
                cols.sort();
                for col in cols {
                    objects.push((format!("column {table}"), col));
                }
            }
            objects.sort();
            objects
        }

        let dir = tempfile::tempdir().unwrap();

        let fresh_path = dir.path().join("fresh.db");
        let fresh = Database::open(&fresh_path).unwrap();
        assert_eq!(fresh.schema_version().unwrap(), SCHEMA_VERSION);

        let upgraded_path = dir.path().join("upgraded.db");
        {
            let conn = Connection::open(&upgraded_path).unwrap();
            conn.execute_batch(OLD_SCHEMA_V4_SQL).unwrap();
            conn.execute_batch(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (4, 1);",
            )
            .unwrap();
        }
        let upgraded = Database::open(&upgraded_path).unwrap();
        assert_eq!(upgraded.schema_version().unwrap(), SCHEMA_VERSION);

        let fresh_shape = shape(&fresh.conn);
        let upgraded_shape = shape(&upgraded.conn);
        let only_fresh: Vec<_> = fresh_shape
            .iter()
            .filter(|o| !upgraded_shape.contains(o))
            .collect();
        let only_upgraded: Vec<_> = upgraded_shape
            .iter()
            .filter(|o| !fresh_shape.contains(o))
            .collect();
        assert!(
            only_fresh.is_empty() && only_upgraded.is_empty(),
            "schema.sql and migrate() have drifted.\n\
             only on a fresh profile: {only_fresh:#?}\n\
             only on an upgraded one: {only_upgraded:#?}"
        );
    }

    /// T-078 (b) prep: a real v6 profile (built from the actual historical
    /// `OLD_SCHEMA_V6_SQL`, not a hand-reduced fixture) gets
    /// `sync_state.last_attempt_at`/`consecutive_failures` added and jumps
    /// straight to `SCHEMA_VERSION` (7), with an existing `sync_state` row
    /// -- including its pre-migration `last_sync_at` -- preserved rather
    /// than dropped or blanked.
    #[test]
    fn migrate_upgrades_v6_profile_to_v7_adding_sync_attempt_tracking_preserving_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(OLD_SCHEMA_V6_SQL).unwrap();
            conn.execute_batch(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (6, 1000);
                 INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('john', 'John Doe', 'john@example.com', 'generic', 1000, 1000);
                 INSERT INTO folders (id, account_id, remote_id, name, kind)
                     VALUES ('inbox', 'john', 'INBOX', 'Inbox', 'inbox');
                 INSERT INTO sync_state (account_id, folder_id, uidvalidity, uidnext, last_sync_at)
                     VALUES ('john', 'inbox', 42, 100, 1000);",
            )
            .unwrap();

            // Sanity: this really is the old shape -- no `last_attempt_at`
            // column yet -- not a fixture that happens to already match v7.
            let mut stmt = conn.prepare("PRAGMA table_info(sync_state)").unwrap();
            let has_attempt = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .filter_map(std::result::Result::ok)
                .any(|name| name == "last_attempt_at");
            assert!(!has_attempt, "fixture must predate last_attempt_at");
        }

        let db = Database::open(&path).unwrap();
        // T-048 (schema v8): this fixture predates v8 too, so opening it now
        // also walks the `current < 8` block -- jumps straight to the
        // *current* SCHEMA_VERSION, not literally 7, same as every other
        // migrate_upgrades_vN_to_vM test in this module already asserts.
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        let mut stmt = db.conn.prepare("PRAGMA table_info(sync_state)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        drop(stmt);
        assert!(
            cols.iter().any(|c| c == "last_attempt_at"),
            "sync_state.last_attempt_at must exist after migrating to v7"
        );
        assert!(
            cols.iter().any(|c| c == "consecutive_failures"),
            "sync_state.consecutive_failures must exist after migrating to v7"
        );

        // The pre-existing sync_state row survived, its old columns are
        // untouched, and the two new ones default to NULL / 0 rather than
        // erroring or losing the row.
        let (last_sync_at, last_attempt_at, consecutive_failures): (i64, Option<i64>, i64) = db
            .conn
            .query_row(
                "SELECT last_sync_at, last_attempt_at, consecutive_failures \
                 FROM sync_state WHERE account_id = 'john' AND folder_id = 'inbox'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(last_sync_at, 1000);
        assert_eq!(last_attempt_at, None);
        assert_eq!(consecutive_failures, 0);

        let migrations_after_first_open: i32 = db
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_first_open, 2, "v6 row plus the new v7 row");
        drop(db);

        // Reopening an already-v7 profile is idempotent: no duplicate
        // migration row, and no "duplicate column name" error from
        // re-running `ADD COLUMN` on columns that are now already there.
        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.schema_version().unwrap(), SCHEMA_VERSION);
        let migrations_after_second_open: i32 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_second_open, 2);
    }

    /// T-048: a real v7 profile (built from `OLD_SCHEMA_V7_SQL`, the
    /// actual historical schema, not a hand-reduced fixture) gains
    /// `fts_pending` and jumps straight to `SCHEMA_VERSION` (8) -- and,
    /// critically, every message that already existed on that profile
    /// (synced before T-048 ever ran, so `messages_fts` never heard about
    /// it -- see `messages_fts`'s doc comment in schema.sql) is backfilled
    /// into `fts_pending` so the background indexer picks it up. Without
    /// this backfill, upgrading an existing profile would only ever index
    /// *new* mail from the moment of the upgrade forward, leaving
    /// everything synced before it permanently unsearchable -- this is
    /// the regression this test exists to catch by name.
    #[test]
    fn migrate_upgrades_v7_profile_to_v8_backfilling_existing_messages_into_fts_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(OLD_SCHEMA_V7_SQL).unwrap();
            conn.execute_batch(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (7, 1000);
                 INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('john', 'John Doe', 'john@example.com', 'generic', 1000, 1000);
                 INSERT INTO folders (id, account_id, remote_id, name, kind)
                     VALUES ('inbox', 'john', 'INBOX', 'Inbox', 'inbox');
                 INSERT INTO threads (id, account_id, folder_id, subject, snippet, date)
                     VALUES ('t1', 'john', 'inbox', 'Hello', '', 1000);
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date)
                     VALUES ('m1', 'john', 't1', 'inbox', 1000);",
            )
            .unwrap();

            // Sanity: this really is the old shape -- no `fts_pending`
            // table yet -- not a fixture that happens to already match v8.
            let table_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'fts_pending'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(table_count, 0, "fixture must predate fts_pending");
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        let pending: Vec<String> = {
            let mut stmt = db
                .conn
                .prepare("SELECT message_id FROM fts_pending ORDER BY message_id")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            pending,
            vec!["m1".to_string()],
            "the pre-existing message must be queued for indexing, not silently left unsearchable"
        );

        let migrations_after_first_open: i32 = db
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_first_open, 2, "v7 row plus the new v8 row");
        drop(db);

        // Reopening an already-v8 profile is idempotent: no duplicate
        // migration row, no "duplicate table"/"UNIQUE constraint failed"
        // error from re-running the backfill, and the message already
        // queued is not queued a second time.
        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.schema_version().unwrap(), SCHEMA_VERSION);
        let pending_after_reopen: i64 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM fts_pending", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending_after_reopen, 1);
        let migrations_after_second_open: i32 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_second_open, 2);
    }

    /// T-093: a real v8 profile (built from `OLD_SCHEMA_V8_SQL`, the actual
    /// historical schema, not a hand-reduced fixture) jumps straight to
    /// `SCHEMA_VERSION` (9), and every message that already has a cached
    /// body (`body_path IS NOT NULL`) gets re-queued into `fts_pending`.
    /// This is not the same backfill T-048 did -- these messages already
    /// went through `messages_fts` once, at v8, when `index_one` still
    /// wrote raw undecoded RFC822 bytes into `body` instead of parsed text
    /// (see the `current < 9` migration block's doc comment). Without this
    /// re-queue, a message synced before T-093 landed would stay
    /// findable only by header/boundary noise forever, since nothing else
    /// ever revisits an already-indexed row.
    ///
    /// The v9 statement itself is still gated on `body_path IS NOT NULL`
    /// (a body-less message indexes to an empty body either way, so queuing
    /// it there would be wasted indexer work). What the *queue* holds after
    /// this open is a different question: a profile this old walks every
    /// later block too, and v29's re-normalisation re-queues every message
    /// including the body-less one, so both ids are expected below.
    #[test]
    fn migrate_upgrades_v8_profile_to_v9_backfilling_the_stale_index_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(OLD_SCHEMA_V8_SQL).unwrap();
            conn.execute_batch(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (8, 1000);
                 INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('john', 'John Doe', 'john@example.com', 'generic', 1000, 1000);
                 INSERT INTO folders (id, account_id, remote_id, name, kind)
                     VALUES ('inbox', 'john', 'INBOX', 'Inbox', 'inbox');
                 INSERT INTO threads (id, account_id, folder_id, subject, snippet, date)
                     VALUES ('t1', 'john', 'inbox', 'Hello', '', 1000);
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date, body_path)
                     VALUES ('m1', 'john', 't1', 'inbox', 1000, '/cache/m1.eml');
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date, body_path)
                     VALUES ('m2', 'john', 't1', 'inbox', 1001, NULL);",
            )
            .unwrap();

            // Sanity: this really is the old shape -- `fts_pending` already
            // exists (T-048 shipped it at v8) but is empty, i.e. neither
            // message has been queued yet by this fixture -- not a
            // fixture that happens to already match v9's post-backfill
            // state.
            let pending_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM fts_pending", [], |r| r.get(0))
                .unwrap();
            assert_eq!(pending_count, 0, "fixture must predate the v9 backfill");
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        let pending: Vec<String> = {
            let mut stmt = db
                .conn
                .prepare("SELECT message_id FROM fts_pending ORDER BY message_id")
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        assert_eq!(
            pending,
            vec!["m1".to_string(), "m2".to_string()],
            "v9 re-queues the cached body; v29, which this profile also \
             walks, re-queues the rest of the index with it"
        );

        let migrations_after_first_open: i32 = db
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_first_open, 2, "v8 row plus the new v9 row");
        drop(db);

        // Reopening an already-v9 profile is idempotent: no duplicate
        // migration row, no "UNIQUE constraint failed" error from
        // re-running the backfill, and the message already queued is not
        // queued a second time.
        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.schema_version().unwrap(), SCHEMA_VERSION);
        let pending_after_reopen: i64 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM fts_pending", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending_after_reopen, 2);
        let migrations_after_second_open: i32 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations_after_second_open, 2);
    }

    /// T-076/T-034: a v9 profile gains durable move intents and the
    /// append-only Undo history/causal column, and reopening it does not
    /// duplicate migration/table/indexes.
    #[test]
    fn migrate_upgrades_v9_profile_to_v11_adding_move_and_undo_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(OLD_SCHEMA_V8_SQL).unwrap();
            conn.execute_batch(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (9, 1000);",
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(db
            .table_names()
            .unwrap()
            .iter()
            .any(|t| t == "operation_moves"));
        assert!(db
            .table_names()
            .unwrap()
            .iter()
            .any(|t| t == "operation_move_history"));
        let operation_columns = db
            .column_names()
            .unwrap()
            .into_iter()
            .filter_map(|(table, column)| (table == "operations").then_some(column))
            .collect::<Vec<_>>();
        assert!(operation_columns.iter().any(|column| column == "undo_of"));
        assert!(operation_columns
            .iter()
            .any(|column| column == "undo_requested_at"));
        let columns = db
            .column_names()
            .unwrap()
            .into_iter()
            .filter_map(|(table, column)| (table == "operation_moves").then_some(column))
            .collect::<Vec<_>>();
        assert_eq!(
            columns,
            vec![
                "operation_id",
                "message_id",
                "source_folder_id",
                "source_remote_id",
                "source_uid",
                "destination_folder_id",
                "destination_remote_id",
                "destination_uid",
            ]
        );
        let index_names: Vec<String> = db
            .conn
            .prepare("PRAGMA index_list(operation_moves)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for index in [
            "operation_moves_source_locator",
            "operation_moves_destination_locator",
            "operation_moves_message",
        ] {
            assert!(
                index_names.iter().any(|name| name == index),
                "missing {index}"
            );
        }
        let fk_targets: Vec<String> = db
            .conn
            .prepare("PRAGMA foreign_key_list(operation_moves)")
            .unwrap()
            .query_map([], |row| row.get(2))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(fk_targets.iter().any(|table| table == "operations"));
        assert!(fk_targets.iter().any(|table| table == "messages"));
        assert_eq!(
            fk_targets
                .iter()
                .filter(|table| *table == "folders")
                .count(),
            2,
            "source and destination folder references must both be present"
        );
        drop(db);

        let reopened = Database::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), SCHEMA_VERSION);
        let migrations: i64 = reopened
            .conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(migrations, 2);
    }

    /// T-060t: a v20 profile gains `folders.delimiter`. The column is what
    /// lets Core rename a nested mailbox without promoting it to the top
    /// level, and it cannot be recovered from `remote_id` -- so the
    /// migration must add it to existing profiles, leave every existing
    /// folder row intact, and leave the new column NULL until the next
    /// `LIST` walk reports a real delimiter.
    #[test]
    fn migrate_upgrades_v20_profile_to_v21_adding_the_folder_delimiter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_migrations (
                     version INTEGER PRIMARY KEY,
                     applied_at INTEGER NOT NULL
                 );
                 CREATE TABLE accounts (id TEXT PRIMARY KEY);
                 CREATE TABLE folders (
                     id TEXT PRIMARY KEY,
                     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                     remote_id TEXT,
                     name TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     parent_id TEXT REFERENCES folders(id),
                     color TEXT,
                     UNIQUE (account_id, remote_id)
                 );
                 INSERT INTO schema_migrations (version, applied_at) VALUES (20, 1000);
                 INSERT INTO accounts (id) VALUES ('you');
                 INSERT INTO folders (id, account_id, remote_id, name, kind)
                 VALUES ('you:ideas', 'you', 'Team/Ideas', 'Ideas', 'custom');",
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let columns = db
            .column_names()
            .unwrap()
            .into_iter()
            .filter_map(|(table, column)| (table == "folders").then_some(column))
            .collect::<Vec<_>>();
        assert!(
            columns.iter().any(|column| column == "delimiter"),
            "v21 must add folders.delimiter, got {columns:?}"
        );
        let (remote_id, delimiter): (String, Option<String>) = db
            .conn
            .query_row(
                "SELECT remote_id, delimiter FROM folders WHERE id = 'you:ideas'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(remote_id, "Team/Ideas", "the existing folder must survive");
        assert_eq!(
            delimiter, None,
            "the delimiter is server-reported: guessing '/' here would be a lie"
        );

        // Reopening must not try to add the column a second time -- a bare
        // ALTER TABLE would error where `add_column_if_missing` does not.
        drop(db);
        let reopened = Database::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), SCHEMA_VERSION);
    }

    /// T-060t: a fresh profile gets `delimiter` straight from `schema.sql`,
    /// so the `current < 21` block has to be a no-op there.
    #[test]
    fn a_fresh_profile_already_has_the_folder_delimiter() {
        let db = Database::memory().unwrap();
        let mut stmt = db.conn.prepare("PRAGMA table_info(folders)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(cols.iter().any(|c| c == "delimiter"), "got {cols:?}");
    }

    /// T-060u: a v21 profile gains `folders.deleted_at`. Six foreign keys
    /// point at `folders(id)` (`folders.parent_id`, `threads`, `messages`,
    /// `sync_state`, and both ends of `operation_moves`), so a deleted
    /// folder can never be a deleted *row* -- it is a tombstone, and this
    /// column is the tombstone. An existing folder must come through the
    /// migration alive, which means NULL here, not 0.
    #[test]
    fn migrate_upgrades_v21_profile_to_v22_adding_the_folder_tombstone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_migrations (
                     version INTEGER PRIMARY KEY,
                     applied_at INTEGER NOT NULL
                 );
                 CREATE TABLE accounts (id TEXT PRIMARY KEY);
                 CREATE TABLE folders (
                     id TEXT PRIMARY KEY,
                     account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                     remote_id TEXT,
                     name TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     parent_id TEXT REFERENCES folders(id),
                     color TEXT,
                     delimiter TEXT,
                     UNIQUE (account_id, remote_id)
                 );
                 INSERT INTO schema_migrations (version, applied_at) VALUES (21, 1000);
                 INSERT INTO accounts (id) VALUES ('you');
                 INSERT INTO folders (id, account_id, remote_id, name, kind)
                 VALUES ('you:ideas', 'you', 'Ideas', 'Ideas', 'custom');",
            )
            .unwrap();
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let (name, deleted_at): (String, Option<i64>) = db
            .conn
            .query_row(
                "SELECT name, deleted_at FROM folders WHERE id = 'you:ideas'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(name, "Ideas", "the existing folder must survive");
        assert_eq!(
            deleted_at, None,
            "every folder that existed before the column did is a live folder"
        );

        drop(db);
        let reopened = Database::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), SCHEMA_VERSION);
    }

    /// T-060u: and a fresh profile gets `deleted_at` from `schema.sql`, so
    /// the `current < 22` block has to be a no-op there.
    #[test]
    fn a_fresh_profile_already_has_the_folder_tombstone() {
        let db = Database::memory().unwrap();
        let mut stmt = db.conn.prepare("PRAGMA table_info(folders)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(cols.iter().any(|c| c == "deleted_at"), "got {cols:?}");
    }

    /// T-096: a v22 profile re-queues every message that has a cached
    /// body, so an HTML-only message indexed with an empty `body` before
    /// the fix becomes findable by its own words instead of staying wrong
    /// forever. The fixture is built by opening a current profile and then
    /// stamping `schema_migrations` back to 22, which is faithful precisely
    /// because v23 adds no DDL at all: a real v22 profile's structure is
    /// byte-for-byte this one's (see `schema.sql`'s T-096 note).
    ///
    /// The v23 statement is gated on `body_path IS NOT NULL` -- a message
    /// with no body on disk has no words to become findable by. The queue
    /// this test reads afterwards holds more than that: a v22 profile also
    /// walks v29, whose re-normalisation re-queues the whole index (see
    /// `schema.sql`'s T-155 note), so the body-less message is expected in
    /// it too.
    #[test]
    fn migrate_upgrades_v22_profile_to_v23_requeueing_the_stale_html_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let db = Database::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                         VALUES ('you', 'You', 'you@example.com', 'generic', 1000, 1000);
                     INSERT INTO folders (id, account_id, remote_id, name, kind)
                         VALUES ('you:inbox', 'you', 'INBOX', 'Inbox', 'inbox');
                     INSERT INTO threads (id, account_id, folder_id, subject, date)
                         VALUES ('t1', 'you', 'you:inbox', 'Receipt', 1000);
                     INSERT INTO messages (id, account_id, thread_id, folder_id, date, body_path)
                         VALUES ('m1', 'you', 't1', 'you:inbox', 1000, 'you/m1.eml');
                     INSERT INTO messages (id, account_id, thread_id, folder_id, date, body_path)
                         VALUES ('m2', 'you', 't1', 'you:inbox', 1000, NULL);
                     DELETE FROM fts_pending;
                     DELETE FROM schema_migrations;
                     INSERT INTO schema_migrations (version, applied_at)
                         VALUES (22, 1000);",
                )
                .unwrap();
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let queued: Vec<String> = db
            .conn
            .prepare("SELECT message_id FROM fts_pending ORDER BY message_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert_eq!(
            queued,
            vec!["m1".to_string(), "m2".to_string()],
            "v23 re-queues the message whose body is on disk; v29, which \
             this profile also walks, re-queues the whole index with it"
        );

        // Idempotent: a second open must not re-run the backfill, and the
        // queue must not grow just because the profile was opened twice.
        drop(db);
        let reopened = Database::open(&path).unwrap();
        assert_eq!(reopened.schema_version().unwrap(), SCHEMA_VERSION);
        let count: i64 = reopened
            .conn
            .query_row("SELECT COUNT(*) FROM fts_pending", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    /// T-111: a v24 profile gains `attachments.cache_bytes` and keeps
    /// everything it already had. The cached attachment's pointer must
    /// survive the upgrade with a NULL size -- that row is exactly the case
    /// `Core::enforce_attachment_cache_limit` counts at `size_bytes`, and
    /// an upgrade that dropped the pointer instead would silently orphan a
    /// file the profile still holds.
    #[test]
    fn migrate_upgrades_v24_profile_to_v25_adding_the_attachment_size_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let db = Database::open(&path).unwrap();
            db.conn
                .execute_batch(
                    "INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                         VALUES ('you', 'You', 'you@example.com', 'generic', 1000, 1000);
                     INSERT INTO folders (id, account_id, remote_id, name, kind)
                         VALUES ('you:inbox', 'you', 'INBOX', 'Inbox', 'inbox');
                     INSERT INTO threads (id, account_id, folder_id, subject, date)
                         VALUES ('t1', 'you', 'you:inbox', 'Receipt', 1000);
                     INSERT INTO messages (id, account_id, thread_id, folder_id, date)
                         VALUES ('m1', 'you', 't1', 'you:inbox', 1000);
                     INSERT INTO attachments
                         (id, account_id, message_id, filename, mime, size_bytes, cache_path)
                         VALUES ('a1', 'you', 'm1', 'file.bin', 'application/pdf', 4000,
                                 'you/a1.bin');
                     ALTER TABLE attachments DROP COLUMN cache_bytes;
                     DELETE FROM schema_migrations;
                     INSERT INTO schema_migrations (version, applied_at)
                         VALUES (24, 1000);",
                )
                .unwrap();
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let mut stmt = db.conn.prepare("PRAGMA table_info(attachments)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(cols.iter().any(|c| c == "cache_bytes"), "got {cols:?}");

        let (cache_path, cache_bytes, size_bytes): (Option<String>, Option<i64>, i64) = db
            .conn
            .query_row(
                "SELECT cache_path, cache_bytes, size_bytes FROM attachments WHERE id = 'a1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(cache_path.as_deref(), Some("you/a1.bin"));
        assert_eq!(
            cache_bytes, None,
            "an old row honestly has no measured size"
        );
        assert_eq!(
            size_bytes, 4000,
            "which is why the sweep falls back to this"
        );
    }

    /// The other half of the same lesson (T-079/T-081), applied to T-078
    /// (b) prep: a database created fresh from `schema.sql` already has
    /// `last_attempt_at`/`consecutive_failures` (the `current < 7` block's
    /// `add_column_if_missing` calls must see that and do nothing) --
    /// checked here by opening a *brand new* profile and confirming both
    /// columns exist and that `migrate()` did not error doing it, which a
    /// bare `ALTER TABLE ... ADD COLUMN` would have.
    #[test]
    fn a_fresh_profile_already_has_sync_attempt_columns() {
        let db = Database::memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let mut stmt = db.conn.prepare("PRAGMA table_info(sync_state)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .collect();
        assert!(cols.iter().any(|c| c == "last_attempt_at"));
        assert!(cols.iter().any(|c| c == "consecutive_failures"));
    }

    /// T-079's rebuild replaces `folders` with a definition written out in
    /// Rust, so it must only touch a profile that actually still carries
    /// the old constraint. A database created fresh from `schema.sql`
    /// already has the right shape; rebuilding it anyway would make the
    /// Rust copy the effective definition of the table, and the next
    /// column added to `folders` in `schema.sql` would then exist on
    /// upgraded profiles and quietly disappear on new ones. Checked on the
    /// stored `CREATE TABLE` text: `schema.sql`'s says `IF NOT EXISTS`,
    /// a table that went through the rebuild's `ALTER TABLE ... RENAME`
    /// never does.
    #[test]
    fn a_fresh_profile_keeps_schema_sqls_own_folders_table() {
        let db = Database::memory().unwrap();
        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'folders'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // schema.sql is the definition of this table, so the text SQLite
        // stored must be schema.sql's own -- byte for byte, minus the
        // `IF NOT EXISTS` SQLite drops when it records a CREATE. Anything
        // the rebuild produced would differ.
        let schema = include_str!("schema.sql");
        let start = schema
            .find("CREATE TABLE IF NOT EXISTS folders (")
            .expect("schema.sql declares folders");
        let end = start + schema[start..].find(");").expect("folders block ends") + 1;
        let expected = schema[start..end].replace("IF NOT EXISTS ", "");
        assert_eq!(
            sql.trim(),
            expected.trim(),
            "a new profile must keep schema.sql's own folders table, not one \
             the v5 rebuild wrote"
        );
    }

    /// v28: a real pre-v28 profile (today's `schema.sql` with the two
    /// `ON DELETE SET NULL` clauses taken back out -- the only difference
    /// v27 had) upgrades in place. `drafts` and `outbox` are rebuilt, every
    /// row survives with its links intact, and the deletes that used to
    /// fail with `FOREIGN KEY constraint failed` now go through: the thread
    /// drops and leaves the reply draft behind with a NULL `thread_id`, and
    /// the already-queued draft can finally be discarded while its frozen
    /// outbox snapshot stays.
    #[test]
    fn migrate_upgrades_v27_profile_to_v28_relaxing_the_draft_links_preserving_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let v27_schema = include_str!("schema.sql")
            .replace(
                "thread_id TEXT REFERENCES threads(id) ON DELETE SET NULL",
                "thread_id TEXT REFERENCES threads(id)",
            )
            .replace(
                "draft_id TEXT REFERENCES drafts(id) ON DELETE SET NULL",
                "draft_id TEXT REFERENCES drafts(id)",
            );
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&v27_schema).unwrap();
            conn.execute_batch(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (27, 1000);
                 INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('john', 'John Doe', 'john@example.com', 'generic', 1000, 1000);
                 INSERT INTO folders (id, account_id, remote_id, name, kind)
                     VALUES ('inbox', 'john', 'INBOX', 'Inbox', 'inbox');
                 INSERT INTO threads (id, account_id, folder_id, subject, snippet, date)
                     VALUES ('t1', 'john', 'inbox', 'Hi', 'Hi', 1000);
                 INSERT INTO drafts (id, account_id, thread_id, in_reply_to, from_addr,
                     to_addr, subject, body, updated_at, sync_revision)
                     VALUES ('draft:john:1', 'john', 't1', '<orig@example.com>',
                         'john@example.com', 'jane@example.com', 'Re: Hi', 'text', 1000, 3);
                 INSERT INTO draft_attachments (id, account_id, draft_id, filename, size_bytes,
                     source_path)
                     VALUES ('att:1', 'john', 'draft:john:1', 'a.pdf', 12, '/tmp/a.pdf');
                 INSERT INTO drafts (id, account_id, from_addr, to_addr, updated_at)
                     VALUES ('draft:john:2', 'john', 'john@example.com', 'jane@example.com', 1000);
                 INSERT INTO outbox (id, account_id, draft_id, to_addr, subject, created_at, status)
                     VALUES ('out:1', 'john', 'draft:john:2', 'jane@example.com', 'Sent', 1000, 'sent');
                 INSERT INTO outbox_attachments (outbox_id, filename, size_bytes, source_path)
                     VALUES ('out:1', 'a.pdf', 12, '/tmp/a.pdf');",
            )
            .unwrap();

            // Sanity: this really is the old shape, not a fixture that
            // already matches v28. Matched against the column definition
            // itself -- schema.sql's comment above that column names the
            // clause too, and sqlite_master stores comments verbatim.
            let drafts_sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name = 'drafts'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!drafts_sql.contains("REFERENCES threads(id) ON DELETE SET NULL"));
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        // Nothing was lost by the copy-drop-rename, including the child
        // rows whose FK briefly pointed at a table that did not exist.
        let (thread_id, in_reply_to, subject, revision): (Option<String>, String, String, i64) = db
            .conn
            .query_row(
                "SELECT thread_id, in_reply_to, subject, sync_revision FROM drafts \
                 WHERE id = 'draft:john:1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(thread_id.as_deref(), Some("t1"));
        assert_eq!(
            (in_reply_to.as_str(), subject.as_str(), revision),
            ("<orig@example.com>", "Re: Hi", 3)
        );
        let attachments: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM draft_attachments", [], |r| r.get(0))
            .unwrap();
        let outbox_attachments: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM outbox_attachments", [], |r| r.get(0))
            .unwrap();
        assert_eq!((attachments, outbox_attachments), (1, 1));

        // The two deletes that used to be impossible.
        db.conn
            .execute("DELETE FROM threads WHERE id = 't1'", [])
            .expect("a folder reset must be able to drop its threads");
        let orphaned: Option<String> = db
            .conn
            .query_row(
                "SELECT thread_id FROM drafts WHERE id = 'draft:john:1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphaned, None, "the draft survives its thread");
        db.conn
            .execute("DELETE FROM drafts WHERE id = 'draft:john:2'", [])
            .expect("a sent draft must still be discardable");
        let snapshot: (Option<String>, String) = db
            .conn
            .query_row(
                "SELECT draft_id, subject FROM outbox WHERE id = 'out:1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            snapshot,
            (None, "Sent".to_string()),
            "the frozen outbox snapshot outlives the draft it came from"
        );

        // Reopening is idempotent: the rebuild is gated on the stored table
        // text, so an already-current profile does not go through it again.
        drop(db);
        let db2 = Database::open(&path).unwrap();
        assert!(!draft_links_lack_on_delete_set_null(&db2.conn).unwrap());
        let drafts: i64 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(drafts, 1);
    }

    /// The v28 rebuild writes a Rust copy of `drafts`/`outbox`, so -- same
    /// hazard as the v5 `folders` rebuild -- a brand new profile must keep
    /// the tables `schema.sql` itself declared, or the next column added
    /// there would silently exist only on upgraded profiles.
    #[test]
    fn a_fresh_profile_keeps_schema_sqls_own_draft_tables() {
        let db = Database::memory().unwrap();
        let schema = include_str!("schema.sql");
        for table in ["drafts", "outbox"] {
            let sql: String = db
                .conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            let start = schema
                .find(&format!("CREATE TABLE IF NOT EXISTS {table} ("))
                .expect("schema.sql declares the table");
            let end = start + schema[start..].find("\n);").expect("the block ends") + 2;
            let expected = schema[start..end].replace("IF NOT EXISTS ", "");
            assert_eq!(
                sql.trim(),
                expected.trim(),
                "a new profile must keep schema.sql's own {table} table, not one \
                 the v28 rebuild wrote"
            );
        }
    }

    /// v29: a real pre-v29 profile (today's `schema.sql` with the three new
    /// columns taken back out) upgrades in place, additively. All three
    /// halves of the version are checked at once because they ship as one
    /// migration: the reconciliation cursor starts NULL ("no walk yet"),
    /// every existing operation gets the `seq` its rowid already implied,
    /// and every message -- body cached or not -- is re-queued for the
    /// re-normalised FTS index.
    #[test]
    fn migrate_upgrades_v28_profile_to_v29_adding_the_cursor_seq_and_reindex_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        // Only the column declarations are removed; the doc comments above
        // them stay in the fixture text and are inert -- SQLite keeps them
        // verbatim in `sqlite_master`, and nothing here reads that.
        let v28_schema = include_str!("schema.sql")
            .replace(
                "    resync_cursor INTEGER,\n    resync_completed_at INTEGER,\n",
                "",
            )
            .replace("    seq INTEGER,\n", "");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&v28_schema).unwrap();
            conn.execute_batch(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (28, 1000);
                 INSERT INTO accounts (id, name, email, provider, created_at, updated_at)
                     VALUES ('john', 'John Doe', 'john@example.com', 'generic', 1000, 1000);
                 INSERT INTO folders (id, account_id, remote_id, name, kind)
                     VALUES ('inbox', 'john', 'INBOX', 'Inbox', 'inbox');
                 INSERT INTO sync_state (account_id, folder_id, uidvalidity, uidnext, last_sync_at)
                     VALUES ('john', 'inbox', 7, 900, 1000);
                 INSERT INTO threads (id, account_id, folder_id, subject, snippet, date)
                     VALUES ('t1', 'john', 'inbox', 'Hi', 'Hi', 1000);
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date, body_path)
                     VALUES ('m:cached', 'john', 't1', 'inbox', 1000, '/tmp/m1');
                 INSERT INTO messages (id, account_id, thread_id, folder_id, date)
                     VALUES ('m:headers-only', 'john', 't1', 'inbox', 1000);
                 INSERT INTO operations (id, account_id, target_id, op, payload_hash, created_at)
                     VALUES ('star:john:t1:a', 'john', 't1', 'star', 'a', 1000);
                 INSERT INTO operations (id, account_id, target_id, op, payload_hash, created_at)
                     VALUES ('archive:john:t1:b', 'john', 't1', 'archive', 'b', 1000);",
            )
            .unwrap();
            // Sanity: this really is the old shape, not a fixture that
            // already matches v29.
            for (table, column) in [
                ("sync_state", "resync_cursor"),
                ("sync_state", "resync_completed_at"),
                ("operations", "seq"),
            ] {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA table_info({table})"))
                    .unwrap();
                let present = stmt
                    .query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .filter_map(std::result::Result::ok)
                    .any(|name| name == column);
                assert!(!present, "fixture must predate {table}.{column}");
            }
        }

        let db = Database::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);

        // T-157: the cursor and its completion stamp start NULL, which is
        // the honest "no walk has started and none has finished" -- the
        // first pass after the upgrade opens a circle.
        let cursor: (Option<i64>, Option<i64>) = db
            .conn
            .query_row(
                "SELECT resync_cursor, resync_completed_at FROM sync_state \
                 WHERE account_id = 'john' AND folder_id = 'inbox'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cursor, (None, None));
        // The rest of the row is untouched by an additive migration.
        let uidnext: i64 = db
            .conn
            .query_row(
                "SELECT uidnext FROM sync_state WHERE folder_id = 'inbox'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(uidnext, 900);

        // T-162: `seq` is backfilled from `rowid`, i.e. the insert order
        // `claim_next` was already applying these two in.
        let seqs: Vec<(String, i64)> = db
            .conn
            .prepare("SELECT id, seq FROM operations ORDER BY seq")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            seqs,
            vec![
                ("star:john:t1:a".to_string(), 1),
                ("archive:john:t1:b".to_string(), 2)
            ],
            "an upgraded queue keeps the order it already had"
        );

        // T-155: every message is re-queued, including the one whose body
        // was never downloaded -- its subject and addresses are indexed
        // too, and the normalisation changed for those as well.
        let queued: Vec<String> = db
            .conn
            .prepare("SELECT message_id FROM fts_pending ORDER BY message_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            queued,
            vec!["m:cached".to_string(), "m:headers-only".to_string()]
        );

        // Reopening an already-current profile re-runs nothing: no second
        // queue entry, and no `seq` handed out again.
        drop(db);
        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.schema_version().unwrap(), SCHEMA_VERSION);
        let pending: i64 = db2
            .conn
            .query_row("SELECT COUNT(*) FROM fts_pending", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 2);
    }

    /// Seeds one account + folder so the FK-shape tests below have parents.
    fn seed_account_and_folder(conn: &Connection) {
        conn.execute(
            "INSERT INTO accounts (id, name, email, provider, status, download_policy, \
             created_at, updated_at) VALUES ('a', 'A', 'a@example.test', 'imap', 'online', \
             'recent', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, remote_id, name, kind) \
             VALUES ('f', 'a', 'INBOX', 'Inbox', 'inbox')",
            [],
        )
        .unwrap();
    }

    /// v28: sync deletes threads all the time -- a UIDVALIDITY reset, the
    /// last message of a thread vanishing on the server -- and a reply
    /// draft pointing at one used to make that `FOREIGN KEY constraint
    /// failed`, rolling the whole sync transaction back on every later
    /// poll. The thread goes, the owner's text stays.
    #[test]
    fn deleting_a_thread_keeps_the_reply_draft_that_pointed_at_it() {
        let db = Database::memory().unwrap();
        let conn = db.conn();
        seed_account_and_folder(conn);
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, date) \
             VALUES ('t', 'a', 'f', 'subject', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO drafts (id, account_id, thread_id, from_addr, updated_at) \
             VALUES ('d', 'a', 't', 'me@example.test', 0)",
            [],
        )
        .unwrap();
        // The exact statement CoreSyncStore::reset_folder runs on a
        // UIDVALIDITY change (crates/core/src/sync_store.rs).
        let deleted = conn.execute(
            "DELETE FROM threads WHERE account_id = ?1 AND folder_id = ?2",
            rusqlite::params!["a", "f"],
        );
        assert!(
            deleted.is_ok(),
            "a folder reset must be able to drop its threads while a draft \
             still references one: {:?}",
            deleted.err()
        );
        let threads: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        let drafts: i64 = conn
            .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(threads, 0);
        assert_eq!(drafts, 1, "the local draft must survive the thread");
    }

    /// v28: an outbox row is a frozen snapshot (T-041/T-045) that never
    /// reads its draft again, so its `draft_id` must not be able to pin
    /// that draft in place forever -- `Core::delete_draft` used to fail
    /// with `FOREIGN KEY constraint failed` for anything already sent.
    #[test]
    fn deleting_a_draft_that_was_already_queued_for_send_is_allowed() {
        let db = Database::memory().unwrap();
        let conn = db.conn();
        seed_account_and_folder(conn);
        conn.execute(
            "INSERT INTO drafts (id, account_id, from_addr, to_addr, updated_at) \
             VALUES ('d', 'a', 'me@example.test', 'you@example.test', 0)",
            [],
        )
        .unwrap();
        // The frozen snapshot Core::queue_draft_send_in writes.
        conn.execute(
            "INSERT INTO outbox (id, account_id, draft_id, to_addr, created_at) \
             VALUES ('outbox:d:0', 'a', 'd', 'you@example.test', 0)",
            [],
        )
        .unwrap();
        let deleted = conn.execute(
            "DELETE FROM drafts WHERE account_id = ?1 AND id = ?2",
            rusqlite::params!["a", "d"],
        );
        assert!(
            deleted.is_ok(),
            "Core::delete_draft must still discard a draft whose outbox \
             snapshot exists: {:?}",
            deleted.err()
        );
        let drafts: i64 = conn
            .query_row("SELECT COUNT(*) FROM drafts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(drafts, 0);
    }

    /// D13 configures `busy_timeout`, but that only helps a transaction
    /// that takes its write lock up front. A DEFERRED transaction which
    /// reads first and writes later loses its upgrade to
    /// `SQLITE_BUSY_SNAPSHOT` the instant a competitor commits underneath
    /// it -- immediately, with the busy handler never consulted, so the
    /// five second timeout buys nothing there. `immediate_transaction_ref`
    /// is the shape that does wait, and this is the guard on that: a
    /// competing writer holds the lock, our writer blocks until it commits,
    /// then reads what the competitor wrote and finishes on top of it.
    #[test]
    fn an_immediate_transaction_waits_out_a_competing_writer() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        const HOLD: Duration = Duration::from_millis(150);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let ours = Database::open(&path).unwrap();
        ours.conn()
            .execute("INSERT INTO settings (key, value) VALUES ('k', '1')", [])
            .unwrap();

        let (locked, lock_held) = mpsc::channel();
        let competitor_path = path.clone();
        let competitor = std::thread::spawn(move || {
            let theirs = Database::open(&competitor_path).unwrap();
            let tx = theirs.immediate_transaction_ref().unwrap();
            tx.execute("UPDATE settings SET value = '2' WHERE key = 'k'", [])
                .unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(HOLD);
            tx.commit().unwrap();
        });
        lock_held.recv().unwrap();

        let start = Instant::now();
        let tx = ours
            .immediate_transaction_ref()
            .expect("a competing writer must be waited out, not reported as an error");
        let seen: String = tx
            .query_row("SELECT value FROM settings WHERE key = 'k'", [], |r| {
                r.get(0)
            })
            .unwrap();
        tx.execute("UPDATE settings SET value = '3' WHERE key = 'k'", [])
            .unwrap();
        tx.commit().unwrap();
        let waited = start.elapsed();
        competitor.join().unwrap();

        assert!(
            waited >= HOLD / 2,
            "the writer returned after {waited:?} without ever blocking, so the \
             competitor's lock was not actually held"
        );
        assert_eq!(
            seen, "2",
            "the waiting writer must read what the competitor committed, not a \
             snapshot from before it"
        );
        let final_value: String = ours
            .conn()
            .query_row("SELECT value FROM settings WHERE key = 'k'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(final_value, "3");
    }

    // What `tokenize = 'unicode61'` in schema.sql does with text that has
    // no word separators, and what T-155 does about it.
    //
    // Left to itself, `unicode61` glues a whole CJK run into one token and
    // keeps Cyrillic "ё" apart from "е", which made a word inside a
    // Japanese sentence and the query `счет` unreachable through the
    // exact-phrase MATCH `Core::search` issues (D54). T-155 did *not*
    // change the tokenizer (no new schema version, no reindex, no trigram):
    // it normalizes the text on both sides instead --
    // `feathermail_search::fts_text`, applied by `index_one` before the
    // INSERT and by `escape_fts_literal` before the MATCH.
    //
    // These two tests are what that strategy rests on at the schema layer:
    // given the normalized spelling, `unicode61` really does find the word.
    // The normalized literals below are written out by hand rather than
    // produced by `fts_text`, because `scripts/check-layering.sh`
    // (`require_no_feathermail_deps db`) forbids this crate from depending
    // on `feathermail-search` -- the schema is the bottom of the stack. The
    // end-to-end coverage, where both sides genuinely share one function,
    // lives in `crates/core/src/search.rs`
    // (`a_word_inside_a_cjk_body_is_found_by_that_word`,
    // `a_cyrillic_yo_message_is_found_whether_the_user_types_yo_or_ye`).
    #[test]
    fn unicode61_finds_a_cjk_word_once_the_run_is_space_separated() {
        let db = Database::memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO messages_fts (sender, recipients, subject, body, \
                 attachment_names, labels, message_id) \
                 VALUES ('a', 'b', '件 名', \
                 '今 日 の 会 議 は 東 京 で 行 わ れ ま し た', '', '', 'm1')",
                [],
            )
            .unwrap();
        let count = |needle: &str| -> i64 {
            db.conn()
                .query_row(
                    "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                    [needle],
                    |r| r.get(0),
                )
                .unwrap()
        };
        // What `fts_text("東京")` produces on the query side: a two-token
        // phrase, which matches those two tokens next to each other.
        assert_eq!(
            count("\"東 京\""),
            1,
            "a word inside a CJK run must be findable once every character is its own token"
        );
        assert_eq!(
            count("\"今 日 の 会 議 は 東 京 で 行 わ れ ま し た\""),
            1,
            "the whole sentence is still a phrase over the same tokens"
        );
        // Still a literal phrase search, not a wildcard: characters that
        // are not adjacent in the text do not match as a phrase.
        assert_eq!(
            count("\"京 東\""),
            0,
            "per-character tokens must not turn D54's literal phrase into a fuzzy match"
        );
    }

    #[test]
    fn unicode61_finds_a_yo_word_once_yo_is_folded_to_ye() {
        let db = Database::memory().unwrap();
        db.conn()
            .execute(
                "INSERT INTO messages_fts (sender, recipients, subject, body, \
                 attachment_names, labels, message_id) \
                 VALUES ('a', 'b', 's', 'Оплатите СЧЕТ пожалуйста', '', '', 'm1')",
                [],
            )
            .unwrap();
        let count = |needle: &str| -> i64 {
            db.conn()
                .query_row(
                    "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                    [needle],
                    |r| r.get(0),
                )
                .unwrap()
        };
        // Case folding for Cyrillic is `unicode61`'s own doing; folding
        // "ё" to "е" is ours, applied to the indexed text above and to the
        // query alike -- so both spellings arrive here as "счет".
        assert_eq!(count("\"счет\""), 1, "case folding for Cyrillic works");
        assert_eq!(
            count("\"счета\""),
            0,
            "folding ё must not make the search a prefix/fuzzy match"
        );
    }

    #[test]
    fn concurrent_opens_of_a_fresh_profile_all_succeed() {
        for round in 0..20 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("mail.db");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let mut handles = Vec::new();
            for _ in 0..8 {
                let path = path.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    Database::open(&path).map(|_| ())
                }));
            }
            for handle in handles {
                let result = handle.join().unwrap();
                assert!(
                    result.is_ok(),
                    "round {round}: a concurrent open of a fresh profile failed: {:?}",
                    result.err()
                );
            }
        }
    }

    #[test]
    fn operations_idempotent_index_exists() {
        let db = Database::memory().unwrap();
        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'operations_idempotent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains("account_id"));
        assert!(sql.contains("payload_hash"));
    }
}
