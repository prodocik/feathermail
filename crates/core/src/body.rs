//! Message bodies: on-disk cache and lazy fetch (T-024, D17).
//!
//! Bodies never live in SQLite -- `messages.body_path` points at a file.
//!
//! ## Layout
//!
//! Cached bodies live under a `bodies/` directory that is a sibling of
//! `mail.db` (D13: one profile, one place on disk), sharded two levels deep
//! by a hash of the message id so a 10k-message profile never puts 10k
//! files in one directory: `bodies/<2 hex chars>/<sanitized message id>.body`.
//! [`default_bodies_dir`] computes the real path the same way
//! `feathermail_db::default_db_path` computes `mail.db`'s.
//!
//! ## The seam (read before wiring this in further)
//!
//! Neither [`Core`] nor `feathermail_db::Database` remembers the path they
//! were opened with -- `Core::open`/`Database::open` take a path and never
//! store it. So every function here takes `bodies_dir: &Path` explicitly
//! rather than deriving it from `self`; the caller is responsible for
//! passing [`default_bodies_dir`] (or a test tempdir) consistently across
//! calls.
//!
//! [`Core::open_body`] is the actual "not cached -> FETCH -> store -> return"
//! path: given a live `M: feathermail_sync::MailboxSession`, it resolves the
//! message's real IMAP folder name (`folders.remote_id`) and UID
//! (`messages.provider_uid`), calls [`feathermail_sync::fetch_body`], and
//! caches the result via [`Core::store_body`]. `Thread::message_id` (see
//! `crates/core/src/model.rs`, populated by `Core::get_thread`) is what a
//! caller uses to get the `MessageId` to hand this.
//!
//! What is still genuinely missing, named rather than faked: nothing in
//! this workspace yet holds a live, authenticated `M` alongside a `Core` for
//! a given account (`crates/core/src/sync_store.rs`'s `CoreSyncStore` has no
//! non-test caller either) -- that connection-lifecycle question, and all
//! UI wiring that would call [`Core::open_body`] when a message is opened,
//! are `crates/app/**` territory and out of bounds for this task. See the
//! report for the precise boundary.
//!
//! ## Cache limit policy (D17, ТЗ §41 "recent vs all")
//!
//! [`Core::store_body`] enforces [`Settings::cache_limit_bytes`] on every
//! write: it reads the total occupied size back from SQLite (`SELECT
//! SUM(body_bytes) ...`, a single indexed-scan-free aggregate over the
//! `messages` table, zero filesystem calls), and if that sum exceeds the
//! limit it evicts (deletes the file, clears `body_path`/`body_bytes`)
//! oldest-`date`-first until back under budget. Eviction is not
//! proactive/scheduled -- it only runs as a side effect of caching a new
//! body, and nothing ever deletes a body for being old: there is no expiry
//! date, only a budget.
//!
//! T-102 added the second key. Ordering by `messages.date` alone answers
//! "which mail is oldest", not "which body does the owner still want": a
//! ten-year-old thread opened a minute ago was the *first* thing evicted,
//! and reopening it went back to the network -- which is what the owner
//! saw as "already-opened mail loads again". So `messages.body_read_at`
//! (schema v24) records the last read, [`Core::lookup_body`] stamps it on
//! every cache hit, and the sweep sorts anything read inside
//! [`BODY_KEEP_AFTER_READ_SECS`] to the very back of the queue. Two
//! deliberate limits: protection orders the queue, it does not lift the
//! budget (if everything is protected, the oldest protected body still
//! goes), and the body whose arrival triggered the sweep is never its own
//! victim. The stamp lives in the read path rather than in `store_body`
//! because `store_body` also runs for the 100-message warm-up: a body
//! nobody looked at must not outrank one the owner opened. An open that
//! *missed* the cache still gets stamped -- the shell re-reads through
//! `lookup_body` once the fetch lands (`on_body_ready`).
//!
//! ### `body_bytes` vs `size_bytes` -- do not conflate
//!
//! `messages.size_bytes` is the server-reported `RFC822.SIZE` (set by the
//! header-ingestion path this task does not touch). `messages.body_bytes`
//! (schema v4, added by this task) is the byte length of the *cached file
//! this crate actually wrote* -- the two can legitimately differ (a
//! provider's advertised size vs. what we chose to fetch/store) and
//! `store_body` never touches `size_bytes`.
//!
//! ### Why summing a column instead of `stat`-ing every file
//!
//! The first version of this cache called `fs::metadata` on every cached
//! body on every `store_body` call to compute occupied bytes -- O(cache
//! size) syscalls on a path that D11 budgets 16ms for, exactly the "10k
//! bodies must not sit on the hot path" problem T-024 exists to avoid. The
//! byte count is now written once, at cache-write time, straight from
//! `contents.len()` (what `write_atomic` is about to persist), so the
//! running total is a single `SUM()` query with no filesystem access at
//! all; the only filesystem calls left on the write path are the write
//! itself and, when the limit is actually exceeded, one `remove_file` per
//! evicted body (proportional to the overage, not the whole cache).
//!
//! ### What if `body_bytes` drifts from the actual file (deleted out from
//! under us, disk cleared by hand, etc.)?
//!
//! The column can only go stale, never invent bytes that were never
//! written -- `store_body` is the only writer, and it always writes the
//! real length it just persisted. If a file later disappears behind our
//! back, [`Core::lookup_body`] notices on the next read of *that* message
//! (a `NotFound` I/O error) and self-heals by clearing that row's
//! `body_path`/`body_bytes` right then, so the `SUM()` total stops
//! overcounting it from that point on. Until that message happens to be
//! looked up again, the total is stale in the safe direction only: it can
//! overcount (a vanished file's bytes still counted as occupied), which
//! makes eviction *more* eager, never less -- it cannot undercount and let
//! the cache grow silently past the limit. A full reconciliation pass
//! (walk the directory, compare against the DB) would fix staleness
//! proactively but reintroduces the O(cache size) filesystem scan this
//! design exists to avoid; deliberately not done here -- flagged in the
//! report as a possible future maintenance task, not wired to anything.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, OptionalExtension};

use crate::error::{CoreError, ErrorCode};
use crate::model::{AccountId, AttachmentEncoding, FolderId, FolderKind, MessageId};
use crate::preview::{preview_from_raw_mime, DEFAULT_PREVIEW_CHARS};
use crate::store::{sql_err, unified_folder_filter, Core};

/// Sub-directory (sibling of `mail.db`) holding cached message bodies.
pub const BODIES_DIR_NAME: &str = "bodies";
/// Sibling cache for transfer-decoded incoming attachments (T-043).
pub const ATTACHMENTS_DIR_NAME: &str = "attachments";

/// How much of a sanitized message id survives into a cache file name.
/// Only there to keep the name readable and inside the filesystem limit --
/// uniqueness comes from the hash appended after it, never from this part.
const MAX_SAFE_ID_CHARS: usize = 100;

/// Real profile path for the body cache: `default_db_path()`'s parent,
/// joined with [`BODIES_DIR_NAME`]. Mirrors
/// `feathermail_db::default_db_path` / the `default_db_path` duplicated in
/// `crates/app/src/shell.rs` (same reasoning as that copy: `feathermail_db`
/// is already a dependency of this crate, so no new coupling is needed).
/// Tests should pass their own tempdir instead of this.
pub fn default_bodies_dir() -> PathBuf {
    match feathermail_db::default_db_path().parent() {
        Some(parent) => parent.join(BODIES_DIR_NAME),
        None => PathBuf::from(BODIES_DIR_NAME),
    }
}

/// Real profile path for attachment cache files. Like [`default_bodies_dir`]
/// this is only the storage location; Core owns the durable relative pointer
/// in `attachments.cache_path`, while service owns the live IMAP transfer.
pub fn default_attachments_dir() -> PathBuf {
    match feathermail_db::default_db_path().parent() {
        Some(parent) => parent.join(ATTACHMENTS_DIR_NAME),
        None => PathBuf::from(ATTACHMENTS_DIR_NAME),
    }
}

/// Result of asking Core whether a message's body is cached.
///
/// Deliberately not `Option<Vec<u8>>` / `Option<String>`: a message whose
/// body has never been fetched and a message whose body is genuinely
/// zero-length both exist, and a caller (the UI) needs to tell them apart
/// -- one means "show a loading state," the other means "render an empty
/// message body."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyLookup {
    /// Nothing has been fetched and cached for this message yet.
    NotCached,
    /// Bytes read back from the cache file, exactly as [`Core::store_body`]
    /// wrote them.
    Cached(Vec<u8>),
}

/// T-097(9): how many of a folder's newest messages the shell keeps warm.
///
/// The owner's number. It is a *cache* target, not a sync target: sync stores
/// headers for the whole folder either way, and this only decides how far
/// back the bodies -- and therefore the card previews and an instant reopen
/// -- reach.
pub const PREFETCH_BODIES: u32 = 100;

/// How long [`Core::lookup_body`]'s read-stamp is willing to wait for the
/// write lock before giving up on it.
///
/// The bytes are already in hand by the time the stamp runs, so any wait
/// here is pure stall for the reader. The warm-up holds the writer for a
/// few hundred milliseconds per body and runs a hundred of them back to
/// back, and the connection-wide `busy_timeout` is five seconds: an open
/// that landed mid-warm-up sat in "Loading" for those five seconds and
/// then -- before the stamp became best effort -- reported an error over
/// a read that had already succeeded. Losing a stamp costs one body its
/// place in the eviction order, which is what "best effort" was always
/// meant to mean.
const STAMP_BUSY_TIMEOUT: Duration = Duration::from_millis(50);

/// T-024 batched: how many bodies the warm-up hands the worker per turn.
///
/// The trade-off is head-of-line time, not bandwidth. One body per turn
/// (what T-104 chose when every body was its own round trip) made the
/// hundred-message warm-up cost about forty seconds of the account's only
/// connection. A whole hundred in one turn would be one round trip but
/// would also be the longest a click could sit behind the warm-up.
///
/// Twenty is the middle: the warm-up drops from ~100 round trips to ~5,
/// and a click that lands mid-chunk waits for one chunk rather than one
/// hundredth of the work. The worker also serves single-message requests
/// ahead of batches, so in practice that wait is only whatever chunk is
/// already on the wire.
pub const PREFETCH_CHUNK: usize = 20;

/// The `busy_timeout` `feathermail_db` configures on every connection.
/// [`Core::lookup_body`] lowers it around the stamp and puts it back.
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// T-102: how long a body the owner actually opened is kept out of the
/// size sweep's reach. The owner's words were "cache what we already opened
/// for a couple of days"; two days is that, in seconds.
///
/// This is a *floor on the eviction order*, not an expiry date -- nothing
/// ever deletes a body because it got old. A cached body lives until the
/// cache is over `Settings::cache_limit_bytes` and something has to go, and
/// this decides what goes last.
pub const BODY_KEEP_AFTER_READ_SECS: i64 = 2 * 24 * 60 * 60;

/// How many queued snippets one [`Core::repair_snippet_batch`] call
/// handles. Same size and the same reasoning as
/// [`crate::search::DEFAULT_INDEX_BATCH`]: one file read per row, so a
/// batch is a short block, and a profile with thousands of damaged rows
/// still drains in a handful of worker turns.
pub const DEFAULT_SNIPPET_REPAIR_BATCH: usize = 200;

/// What one [`Core::repair_snippet_batch`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnippetRepairResult {
    /// Rows whose stored snippet actually changed.
    pub repaired: usize,
    /// Rows still queued immediately after this call.
    pub remaining: usize,
}

impl Core {
    /// T-097(9): which of `folder`'s newest `window` messages still have no
    /// cached body, newest first.
    ///
    /// The window is the point, and it is what this used to get wrong. The
    /// query was "the newest `limit` messages that have no body", which is
    /// not the same set: once the first hundred are cached it returns the
    /// hundred *after* them, so re-running the warm-up walked backwards
    /// into the archive instead of picking up what had just arrived. Asking
    /// the window first and filtering inside it makes the call idempotent
    /// -- everything warm means an empty answer -- so it can safely be run
    /// again whenever the folder changes, which is exactly when new mail
    /// needs warming.
    ///
    /// Only rows that can actually be fetched are returned: a NULL
    /// `provider_uid` is a message that has never been matched to a server
    /// UID, and handing one to a fetch would be a request built from a
    /// fabricated identifier -- the same class of mistake `open_body` refuses
    /// for a NULL `folders.remote_id`.
    ///
    /// Ordering by `date DESC` rather than by UID is deliberate: the caller
    /// is filling in what the reader is looking at, and the list is sorted by
    /// date. A folder whose UIDs and dates disagree (a bulk import, a moved
    /// message) would otherwise warm the wrong end of it.
    pub fn messages_needing_warmup(
        &self,
        folder: &FolderId,
        window: u32,
    ) -> Result<Vec<MessageId>, CoreError> {
        let mut stmt = self
            .db
            .conn()
            .prepare(
                "SELECT id FROM ( \
                     SELECT id, body_path, provider_uid FROM messages \
                     WHERE folder_id = ?1 \
                     ORDER BY date DESC, id DESC \
                     LIMIT ?2 \
                 ) WHERE body_path IS NULL AND provider_uid IS NOT NULL",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map(params![folder.as_str(), window], |row| {
                row.get::<_, String>(0).map(MessageId)
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(sql_err)?);
        }
        Ok(out)
    }

    /// T-108: the merged view's half of [`Self::messages_needing_warmup`].
    ///
    /// The merged view is the same mailbox read across every account, so it
    /// warms the same way -- what it cannot do is answer "which account"
    /// from the folder id, because there isn't one. So the pairs come back
    /// with the message: the worker is asked per account, and the caller
    /// has the account in hand without having to look it up per id.
    ///
    /// The window spans the merge rather than each account separately, for
    /// the same reason the list does: a hundred rows on screen are a
    /// hundred rows, no matter how they split between mailboxes.
    pub fn messages_needing_warmup_unified(
        &self,
        kind: FolderKind,
        window: u32,
    ) -> Result<Vec<(AccountId, MessageId)>, CoreError> {
        let Some(filter) = unified_folder_filter(kind) else {
            return Err(CoreError::from_code(ErrorCode::InvalidArgument));
        };
        let sql = format!(
            "SELECT account_id, id FROM ( \
                 SELECT m.account_id AS account_id, m.id AS id, \
                        m.body_path AS body_path, m.provider_uid AS provider_uid \
                 FROM messages m JOIN threads t ON t.id = m.thread_id \
                 WHERE {filter} \
                 ORDER BY m.date DESC, m.id DESC \
                 LIMIT ?1 \
             ) WHERE body_path IS NULL AND provider_uid IS NOT NULL"
        );
        let conn = self.db.conn();
        let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
        let rows = stmt
            .query_map(params![window], |row| {
                Ok((
                    AccountId(row.get::<_, String>(0)?),
                    MessageId(row.get::<_, String>(1)?),
                ))
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(sql_err)?);
        }
        Ok(out)
    }

    /// Ask whether `id`'s body is cached, reading it back from disk if so.
    ///
    /// `Err(MessageNotFound)` only when `id` has no row in `messages` at
    /// all. A `body_path` that points at a file which is no longer there
    /// (cleared out from under us, disk wiped, whatever) is treated the
    /// same as "never cached" -- [`BodyLookup::NotCached`] -- rather than a
    /// hard error: a stale pointer self-heals right here (clearing
    /// `body_path`/`body_bytes` for this row, so the cache-size `SUM()`
    /// stops overcounting it -- see the module doc's note on `body_bytes`
    /// drift), and surfacing an error for something a re-fetch fixes would
    /// just be a worse user experience for no benefit. Any other read
    /// failure (permission denied, I/O error) is surfaced as a real error
    /// instead, since a re-fetch is unlikely to fix that. Takes `&mut self`
    /// (not `&self`) precisely because of that self-heal write.
    pub fn lookup_body(
        &mut self,
        id: &MessageId,
        bodies_dir: &Path,
    ) -> Result<BodyLookup, CoreError> {
        let row: Option<Option<String>> = self
            .db
            .conn()
            .query_row(
                "SELECT body_path FROM messages WHERE id = ?1",
                params![id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(read_err)?;
        let rel = match row {
            None => return Err(CoreError::from_code(ErrorCode::MessageNotFound)),
            Some(None) => return Ok(BodyLookup::NotCached),
            Some(Some(rel)) => rel,
        };
        match fs::read(bodies_dir.join(&rel)) {
            Ok(bytes) => {
                // T-102: this is the read the eviction policy needs to know
                // about. Stamping here rather than in `store_body` is
                // deliberate: `store_body` also runs for the 100-message
                // warm-up, and a prefetched body nobody looked at must not
                // outrank one the owner actually opened.
                self.stamp_body_read(id);
                Ok(BodyLookup::Cached(bytes))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // The pointer outlived its file, so the honest answer is
                // `NotCached` and the worker will fetch the body again.
                // Clearing the stale pointer is bookkeeping on top of that
                // answer, not a precondition for it: propagating a busy
                // writer from here would turn "this needs refetching" into
                // the same false error the read-stamp used to raise.
                let _ = self.db.conn().execute(
                    "UPDATE messages SET body_path = NULL, body_bytes = NULL WHERE id = ?1",
                    params![id.as_str()],
                );
                Ok(BodyLookup::NotCached)
            }
            Err(e) => Err(io_err("read cached body", e)),
        }
    }

    /// T-102: record that `id`'s cached body was just read. Best effort by
    /// design -- a failed stamp must not turn a successful read into an
    /// error, so the only thing a lost update costs is that this body sorts
    /// as "not recently opened" in the next sweep.
    ///
    /// It took an owner report to make the code say what this comment
    /// already did. The stamp used to propagate its error with `?`, and
    /// because it is a *write* on the path that *reads* a letter, the
    /// warm-up's writer was enough to lose the race: the open sat five
    /// seconds on the connection's `busy_timeout` and then showed
    /// "Something went wrong reading this message from the local cache"
    /// over bytes that were already in hand. Hence both halves here --
    /// the error is dropped, and [`STAMP_BUSY_TIMEOUT`] keeps the attempt
    /// from being a wait anyone can feel.
    fn stamp_body_read(&mut self, id: &MessageId) {
        let now = self.now();
        let conn = self.db.conn();
        // Try for the write lock, do not queue for it: see
        // `STAMP_BUSY_TIMEOUT`. Both the lowering and the restore are
        // themselves best effort -- a connection that will not take a
        // pragma is in no state to be stamped either, and the read that
        // brought us here has already succeeded.
        if conn.busy_timeout(STAMP_BUSY_TIMEOUT).is_err() {
            return;
        }
        let _ = conn.execute(
            "UPDATE messages SET body_read_at = ?2 WHERE id = ?1",
            params![id.as_str(), now],
        );
        let _ = conn.busy_timeout(DEFAULT_BUSY_TIMEOUT);
    }

    /// Cache `contents` as `id`'s body: write it to disk (atomically -- a
    /// crash or a truncated write can never leave a half-written file that
    /// later passes as a valid cache entry) and point `messages.body_path`
    /// at it. Then enforce the cache-size limit (see the module doc).
    ///
    /// `Err(MessageNotFound)` if `id` has no row in `messages`.
    pub fn store_body(
        &mut self,
        id: &MessageId,
        bodies_dir: &Path,
        contents: &[u8],
    ) -> Result<(), CoreError> {
        // Reject a missing id before touching the filesystem. The durable
        // owner is deliberately loaded again inside the write transaction:
        // another Core handle can move/rethread/remove this message between
        // this check and the cache-file write.
        let exists: Option<i64> = self
            .db
            .conn()
            .query_row(
                "SELECT 1 FROM messages WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if exists.is_none() {
            return Err(CoreError::from_code(ErrorCode::MessageNotFound));
        }

        let rel = body_rel_path(id);
        let full = bodies_dir.join(&rel);
        write_atomic(&full, contents).map_err(|e| io_err("write cached body", e))?;

        // body_bytes is the length of what was *just written* -- not a
        // `size_bytes` reference, not a stat() of the file we just made
        // (which would defeat the point: we already know the length, it's
        // `contents.len()`). This is the only place this column is ever
        // written, which is what keeps the SUM() in
        // `enforce_body_cache_limit` trustworthy.
        let rel_str = rel.to_string_lossy().into_owned();
        let body_bytes = contents.len() as i64;
        // Keep the row's bounded preview beside the cached body. Updating
        // message and thread snippets is content-only database work; no
        // body text reaches logs, errors, or UI debug output. The path,
        // message preview, and latest-thread projection commit together so a
        // crash cannot leave the cache metadata half-updated.
        let snippet = preview_from_raw_mime(contents, DEFAULT_PREVIEW_CHARS);
        let tx = self.db.conn().unchecked_transaction().map_err(sql_err)?;
        Self::store_cached_body_metadata(&tx, id, &rel_str, body_bytes, &snippet, contents)?;
        tx.commit().map_err(sql_err)?;

        // T-048: whatever `messages_fts` already has for this message (if
        // anything -- it may still be sitting unindexed) was built without
        // this body text. Queue it for the background indexer to pick up
        // rather than indexing inline here, so opening a thread to fetch
        // its body never blocks on FTS work. See `crate::search`'s module
        // doc for the full "two cases, one queue" reasoning.
        crate::search::enqueue_for_indexing(self.db.conn(), id.as_str()).map_err(sql_err)?;

        let limit = self.settings().cache_limit_bytes;
        // T-102: the body just written is never its own victim. Without this
        // a cache already full of protected bodies would evict the message
        // the reader is opening *right now*, the shell would re-read it,
        // find nothing, and fetch it again -- a loop over the network for a
        // message that was on disk a millisecond earlier.
        self.enforce_body_cache_limit_keeping(bodies_dir, limit, Some(id))?;
        Ok(())
    }

    /// T-134: recompute up to `limit` queued snippets from the cached MIME
    /// bodies they were built from, and drop those rows from the queue.
    ///
    /// The owner: "в карточке письма в некоторых письмах где есть картинки
    /// пишет &&&&&&&& за тем текст письма". `preview::html_to_text`
    /// collected entities *inside* tags, so an image URL's `&`-separated
    /// tracking parameters emptied into the preview. That parser is fixed,
    /// but a snippet is a stored projection: rows written by the broken one
    /// stay broken until something recomputes them. `Database::migrate`'s
    /// v26 block queues the damaged rows; this drains the queue.
    ///
    /// Bounded and drained by the background worker for the same reason
    /// [`Core::index_pending_batch`] is (see `crates/service`'s worker doc):
    /// one file read per row is disk work, and D11 forbids it on the GTK
    /// thread. Every row handled leaves the queue whether or not its
    /// snippet actually changed -- a preview whose `&&` is genuine, or a
    /// body that has since been evicted, must not be looked at forever.
    ///
    /// The message row and the thread projection are updated in one
    /// transaction, through the same `UPDATE_LATEST_THREAD_SNIPPET_SQL` the
    /// cache write uses, so a thread never shows a preview its own latest
    /// message no longer has.
    pub fn repair_snippet_batch(
        &self,
        bodies_dir: &Path,
        limit: usize,
    ) -> Result<SnippetRepairResult, CoreError> {
        let conn = self.db.conn();
        let ids: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT message_id FROM snippet_repairs ORDER BY message_id LIMIT ?1")
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![limit as i64], |row| row.get::<_, String>(0))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            rows
        };
        if ids.is_empty() {
            return Ok(SnippetRepairResult::default());
        }

        let mut repaired = 0usize;
        for id in &ids {
            let row: Option<(Option<String>, String, String, String)> = conn
                .query_row(
                    "SELECT body_path, snippet, account_id, thread_id FROM messages WHERE id = ?1",
                    params![id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()
                .map_err(read_err)?;
            let fixed = match &row {
                Some((Some(rel), stored, account_id, thread_id)) => {
                    // A missing file is not an error here: the cache sweep
                    // is allowed to have evicted this body, and the next
                    // fetch will write a correct snippet on its own.
                    match fs::read(bodies_dir.join(rel)) {
                        Ok(bytes) => {
                            let snippet = preview_from_raw_mime(&bytes, DEFAULT_PREVIEW_CHARS);
                            (&snippet != stored)
                                .then(|| (snippet, account_id.clone(), thread_id.clone()))
                        }
                        Err(_) => None,
                    }
                }
                _ => None,
            };
            let tx = conn.unchecked_transaction().map_err(sql_err)?;
            if let Some((snippet, account_id, thread_id)) = fixed {
                tx.execute(
                    "UPDATE messages SET snippet = ?1 WHERE id = ?2",
                    params![snippet, id],
                )
                .map_err(sql_err)?;
                tx.execute(
                    Self::UPDATE_LATEST_THREAD_SNIPPET_SQL,
                    params![snippet, account_id, thread_id, id],
                )
                .map_err(sql_err)?;
                repaired += 1;
            }
            tx.execute(
                "DELETE FROM snippet_repairs WHERE message_id = ?1",
                params![id],
            )
            .map_err(sql_err)?;
            tx.commit().map_err(sql_err)?;
        }

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM snippet_repairs", [], |row| row.get(0))
            .map_err(sql_err)?;
        Ok(SnippetRepairResult {
            repaired,
            remaining: remaining.max(0) as usize,
        })
    }

    /// These updates deliberately identify exactly one known thread. The
    /// previous correlated outer predicate let SQLite scan all threads in a 10k
    /// profile for one cached-body write (T-068); the accompanying EXPLAIN test
    /// pins the primary-key access path.
    pub(crate) const UPDATE_LATEST_THREAD_SNIPPET_SQL: &str = "UPDATE threads
     SET snippet = ?1
     WHERE account_id = ?2
       AND id = ?3
       AND ?4 = (SELECT latest.id FROM messages latest
                 WHERE latest.account_id = ?2
                   AND latest.thread_id = ?3
                 ORDER BY latest.date DESC, latest.id DESC LIMIT 1)";

    const UPDATE_THREAD_HAS_ATTACHMENT_SQL: &str = "UPDATE threads
     SET has_attachment = COALESCE(
         (SELECT MAX(m.has_attachment) FROM messages m
          WHERE m.account_id = ?1 AND m.thread_id = ?2), 0
     )
     WHERE account_id = ?1 AND id = ?2";

    /// Commit the cache pointer and every projection derived from its raw MIME
    /// body under one SQLite transaction. The owner read is intentionally inside
    /// `tx`: another Core handle may move/rethread the message after the caller
    /// has started writing the cache file, and the current thread—not a stale
    /// pre-write owner—must receive its preview and attachment aggregate.
    fn store_cached_body_metadata(
        tx: &rusqlite::Transaction<'_>,
        id: &MessageId,
        rel_str: &str,
        body_bytes: i64,
        snippet: &str,
        contents: &[u8],
    ) -> Result<(), CoreError> {
        let message_owner: Option<(String, String)> = tx
            .query_row(
                "SELECT account_id, thread_id FROM messages WHERE id = ?1",
                params![id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let (account_id, thread_id) =
            message_owner.ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))?;

        tx.execute(
            "UPDATE messages SET body_path = ?1, body_bytes = ?2, snippet = ?3 WHERE id = ?4",
            params![rel_str, body_bytes, snippet, id.as_str()],
        )
        .map_err(sql_err)?;
        record_attachment_metadata(tx, id, &account_id, &thread_id, contents)?;
        tx.execute(
            Self::UPDATE_LATEST_THREAD_SNIPPET_SQL,
            params![snippet, account_id, thread_id, id.as_str()],
        )
        .map_err(sql_err)?;
        Ok(())
    }

    /// Sum `messages.body_bytes` (one aggregate query, zero filesystem
    /// calls -- see the module doc) and, if it exceeds `limit_bytes`,
    /// delete cache files oldest-`date`-first (clearing
    /// `messages.body_path`/`body_bytes` as each one goes) until back under
    /// budget. Returns how many bodies were evicted. Safe to call on its
    /// own (not just from [`Core::store_body`]) -- e.g. after the user
    /// lowers the cache limit in settings, or from a future periodic
    /// maintenance pass; nothing currently calls it that way (see the
    /// report).
    pub fn enforce_body_cache_limit(
        &mut self,
        bodies_dir: &Path,
        limit_bytes: u64,
    ) -> Result<usize, CoreError> {
        self.enforce_body_cache_limit_keeping(bodies_dir, limit_bytes, None)
    }

    /// The sweep proper. `keep` is the body whose arrival triggered this
    /// pass, if any: it is excluded from the candidates (see `store_body`).
    fn enforce_body_cache_limit_keeping(
        &mut self,
        bodies_dir: &Path,
        limit_bytes: u64,
        keep: Option<&MessageId>,
    ) -> Result<usize, CoreError> {
        let total: i64 = self
            .db
            .conn()
            .query_row(
                "SELECT COALESCE(SUM(body_bytes), 0) FROM messages WHERE body_path IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        let mut total = total.max(0) as u64;

        if total <= limit_bytes {
            return Ok(0);
        }

        // Only reached when we actually must evict -- from here on,
        // filesystem work (and row reads) are proportional to the overage,
        // not the whole cache.
        // T-102: what the owner opened survives the sweep for
        // `BODY_KEEP_AFTER_READ_SECS`. The first ORDER BY key puts protected
        // rows last, so they are only touched when evicting everything else
        // still leaves the cache over budget -- an overflowing cache has to
        // give way to a bounded one, but it gives way in the right order.
        // Within each group the old key stands: oldest mail first.
        let cutoff = self.now().saturating_sub(BODY_KEEP_AFTER_READ_SECS);
        let rows: Vec<(String, String, Option<i64>)> = {
            let conn = self.db.conn();
            let mut stmt = conn
                .prepare(
                    "SELECT id, body_path, body_bytes FROM messages \
                     WHERE body_path IS NOT NULL \
                     ORDER BY (body_read_at IS NOT NULL AND body_read_at > ?1) ASC, \
                              date ASC, id ASC",
                )
                .map_err(sql_err)?;
            let out = stmt
                .query_map(params![cutoff], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            out
        };

        let mut evicted = Vec::new();
        for (id, rel, size) in rows {
            if total <= limit_bytes {
                break;
            }
            if keep.is_some_and(|k| k.as_str() == id) {
                continue;
            }
            let _ = fs::remove_file(bodies_dir.join(&rel));
            total = total.saturating_sub(size.unwrap_or(0).max(0) as u64);
            evicted.push(id);
        }

        if !evicted.is_empty() {
            let conn = self.db.conn();
            let tx = conn.unchecked_transaction().map_err(sql_err)?;
            for id in &evicted {
                tx.execute(
                    "UPDATE messages SET body_path = NULL, body_bytes = NULL WHERE id = ?1",
                    params![id],
                )
                .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
        }
        Ok(evicted.len())
    }

    /// T-111: the same budget discipline for downloaded attachment files.
    ///
    /// Until this existed the attachment cache had no ceiling at all: only
    /// bodies were ever swept, so a profile that saved a few hundred files
    /// grew until the disk said no. The shape deliberately mirrors
    /// [`Core::enforce_body_cache_limit`] -- sum from SQLite, evict only
    /// when over budget, delete the file and clear the pointer in one
    /// transaction -- with three differences worth naming:
    ///
    /// * The budget is [`Settings::attachment_cache_limit_bytes`], its own
    ///   number rather than a share of the body one (see that constant).
    /// * The size column is `attachments.cache_bytes`, written once by
    ///   [`Core::mark_attachment_cached`] from the file it accepted. A row
    ///   cached before schema v25 has NULL there and is counted at
    ///   `size_bytes` instead: that is the server's octet count for the
    ///   *encoded* part, so it over-states a base64 file by about a third.
    ///   Over-stating is the safe direction -- the sweep runs a little
    ///   early rather than a little late -- and each such row is corrected
    ///   the next time it is downloaded.
    /// * There is no read-time protection yet, unlike bodies' T-102 key.
    ///   Nothing stamps "the owner opened this attachment" today, so the
    ///   order is the honest one this schema can answer: oldest mail first.
    ///
    /// Only the cached *file* goes. The `attachments` row -- filename, mime,
    /// size, the IMAP part path it can be fetched from again -- stays, which
    /// is what keeps an evicted attachment re-downloadable and the mail
    /// metadata intact.
    ///
    /// Returns how many files were evicted. Safe to call on its own.
    pub fn enforce_attachment_cache_limit(
        &mut self,
        attachments_dir: &Path,
        limit_bytes: u64,
    ) -> Result<usize, CoreError> {
        self.enforce_attachment_cache_limit_keeping(attachments_dir, limit_bytes, None)
    }

    /// The sweep proper. `keep` is the attachment whose download triggered
    /// this pass, if any: it is never its own victim, for the same reason
    /// [`Core::store_body`] excludes the body it just wrote.
    pub(crate) fn enforce_attachment_cache_limit_keeping(
        &mut self,
        attachments_dir: &Path,
        limit_bytes: u64,
        keep: Option<&str>,
    ) -> Result<usize, CoreError> {
        let total: i64 = self
            .db
            .conn()
            .query_row(
                "SELECT COALESCE(SUM(COALESCE(cache_bytes, size_bytes)), 0) \
                 FROM attachments WHERE cache_path IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .map_err(sql_err)?;
        let mut total = total.max(0) as u64;

        if total <= limit_bytes {
            return Ok(0);
        }

        // Only reached when something must go, so the row read and the
        // unlink()s below are proportional to the overage, not to the cache.
        // The join is what makes "oldest mail first" mean the mail's date
        // rather than the row order; an attachment whose message somehow has
        // no date sorts first, which is the same "unknown goes first" rule
        // the body sweep gives a never-read body.
        let rows: Vec<(String, String, Option<i64>, Option<i64>)> = {
            let conn = self.db.conn();
            let mut stmt = conn
                .prepare(
                    "SELECT a.id, a.cache_path, a.cache_bytes, a.size_bytes \
                     FROM attachments a \
                     LEFT JOIN messages m ON m.id = a.message_id \
                     WHERE a.cache_path IS NOT NULL \
                     ORDER BY m.date ASC, a.id ASC",
                )
                .map_err(sql_err)?;
            let out = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            out
        };

        let mut evicted = Vec::new();
        for (id, rel, cached, declared) in rows {
            if total <= limit_bytes {
                break;
            }
            if keep.is_some_and(|k| k == id) {
                continue;
            }
            let _ = fs::remove_file(attachments_dir.join(&rel));
            let counted = cached.or(declared).unwrap_or(0).max(0) as u64;
            total = total.saturating_sub(counted);
            evicted.push(id);
        }

        if !evicted.is_empty() {
            let conn = self.db.conn();
            let tx = conn.unchecked_transaction().map_err(sql_err)?;
            for id in &evicted {
                tx.execute(
                    "UPDATE attachments SET cache_path = NULL, cache_bytes = NULL \
                     WHERE id = ?1",
                    params![id],
                )
                .map_err(sql_err)?;
            }
            tx.commit().map_err(sql_err)?;
        }
        Ok(evicted.len())
    }

    /// The seam itself (T-024, F3): return `id`'s body, fetching it over
    /// `session` and caching it on the way through when it isn't already
    /// cached. `M: feathermail_sync::MailboxSession` rather than a concrete
    /// IMAP type keeps this crate honest about D9 -- it depends on
    /// `feathermail-sync`'s trait (already a dependency, see
    /// `crates/core/Cargo.toml`), never on `feathermail-providers` or raw
    /// IMAP.
    ///
    /// Steps: [`Core::lookup_body`] first (cache hit short-circuits
    /// everything below, no network touched); on a miss, resolve the
    /// message's `folders.remote_id` (the real IMAP mailbox name --
    /// `folders.id`/`name` are local identity/display, see
    /// `crates/core/src/remote.rs`) and `messages.provider_uid` (the IMAP
    /// UID) by joining the two tables; call
    /// [`feathermail_sync::fetch_body`] (which selects the folder before
    /// fetching, same discipline as `sync_folder`); [`Core::store_body`]
    /// the bytes (which also runs the cache-limit sweep, see above); return
    /// them.
    ///
    /// `Err(MessageNotFound)` if `id` has no row. A row whose `provider_uid`
    /// is `NULL` (message not yet synced from the server, or a purely
    /// local/draft-adjacent row) surfaces as `Err(InvalidArgument)` rather
    /// than attempting a fetch with a made-up UID, and so does a row whose
    /// folder has no `remote_id` yet -- there is no honest mailbox name to
    /// `SELECT` in that case.
    ///
    /// **What this does not do** (named, not silently skipped -- see the
    /// report): it does not obtain `session` itself. Getting a live,
    /// already-authenticated `M` for a given account -- and picking which
    /// account/session goes with which `id` -- is connection-lifecycle
    /// work that does not exist anywhere in this workspace yet (there is
    /// no code today that holds an open `ImapSession` alongside a `Core`;
    /// `crates/core/src/sync_store.rs`'s `CoreSyncStore` has no live
    /// caller either, only tests against a fake session). That is UI/shell
    /// wiring territory (`crates/app/**`), explicitly out of bounds for
    /// this task; the caller this method is written for is expected to
    /// already have both a `Core` and a live `M` in hand.
    pub fn open_body<M: feathermail_sync::MailboxSession>(
        &mut self,
        session: &mut M,
        id: &MessageId,
        bodies_dir: &Path,
    ) -> Result<Vec<u8>, CoreError> {
        if let BodyLookup::Cached(bytes) = self.lookup_body(id, bodies_dir)? {
            return Ok(bytes);
        }

        let row: Option<(Option<String>, Option<i64>)> = self
            .db
            .conn()
            .query_row(
                "SELECT f.remote_id, m.provider_uid \
                 FROM messages m JOIN folders f ON f.id = m.folder_id \
                 WHERE m.id = ?1",
                params![id.as_str()],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let (folder_remote_id, provider_uid) =
            row.ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))?;

        // A folder whose `remote_id` is still NULL has never been matched
        // against a server LIST (T-077) -- a locally created folder that
        // has not been pushed yet, or a seeded placeholder. Defaulting it
        // to `""` would send the server a `SELECT ""`, which is a
        // fabricated mailbox name: the same class of mistake as fetching
        // with a made-up UID, so it gets the same treatment rather than a
        // network round trip that fails for a misleading reason.
        let folder_remote_id = folder_remote_id.filter(|s| !s.is_empty()).ok_or_else(|| {
            CoreError::new(
                ErrorCode::InvalidArgument,
                "That message's folder hasn't been matched to the server yet.",
            )
            .with_details("folders.remote_id missing (NULL or empty)")
        })?;

        let uid: u32 = provider_uid
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| {
                CoreError::new(
                    ErrorCode::InvalidArgument,
                    "That message hasn't been synced from the server yet.",
                )
                .with_details("provider_uid missing (NULL) or out of range")
            })?;

        let bytes =
            feathermail_sync::fetch_body(session, &folder_remote_id, uid).map_err(sync_err)?;

        self.store_body(id, bodies_dir, &bytes)?;
        Ok(bytes)
    }
}

impl Core {
    /// Warm several bodies at once, grouped so each folder costs one
    /// `UID FETCH` instead of one per message (T-024 batched).
    ///
    /// This is the warm-up's door, not a click's: it reports per message
    /// rather than returning bytes, because the caller is filling a cache
    /// and nobody is waiting to read what comes back. A click still goes
    /// through [`Self::open_body`], which is one message and returns it.
    ///
    /// Never fails as a whole. Anything that cannot be fetched -- already
    /// cached, no `provider_uid`, a folder never matched to the server, a
    /// group whose fetch errored -- comes back as `false` for that id and
    /// the rest still run. A warm-up that gave up on the first unusable
    /// row would leave the folder cold for a reason the reader cannot see
    /// or act on.
    pub fn warm_bodies<M: feathermail_sync::MailboxSession>(
        &mut self,
        session: &mut M,
        ids: &[MessageId],
        bodies_dir: &Path,
    ) -> Vec<(MessageId, bool)> {
        let mut done: Vec<(MessageId, bool)> = Vec::with_capacity(ids.len());
        // Folder remote id -> the (uid, message id) pairs to ask it for.
        let mut groups: Vec<(String, Vec<(u32, MessageId)>)> = Vec::new();
        for id in ids {
            match self.warm_target(id, bodies_dir) {
                WarmTarget::Fetch { folder, uid } => {
                    match groups.iter_mut().find(|(f, _)| *f == folder) {
                        Some((_, rows)) => rows.push((uid, id.clone())),
                        None => groups.push((folder, vec![(uid, id.clone())])),
                    }
                }
                WarmTarget::AlreadyWarm => done.push((id.clone(), true)),
                WarmTarget::Unfetchable => done.push((id.clone(), false)),
            }
        }

        for (folder, rows) in groups {
            let uids: Vec<u32> = rows.iter().map(|(uid, _)| *uid).collect();
            let fetched = match feathermail_sync::fetch_bodies(session, &folder, &uids) {
                Ok(fetched) => fetched,
                Err(_) => {
                    // One dead folder does not condemn the others: the next
                    // group still gets its chance, and every id in this one
                    // is reported honestly as not warmed.
                    done.extend(rows.into_iter().map(|(_, id)| (id, false)));
                    continue;
                }
            };
            for (uid, id) in rows {
                let bytes = fetched
                    .iter()
                    .find(|(got, _)| *got == uid)
                    .map(|(_, bytes)| bytes);
                let ok = match bytes {
                    // A UID the server did not return is a message that is
                    // no longer there; the next sync notices, and warming
                    // is not the place to decide that.
                    None => false,
                    Some(bytes) => self.store_body(&id, bodies_dir, bytes).is_ok(),
                };
                done.push((id, ok));
            }
        }
        done
    }

    /// What [`Self::warm_bodies`] can do about one message, decided before
    /// anything touches the network.
    fn warm_target(&mut self, id: &MessageId, bodies_dir: &Path) -> WarmTarget {
        if let Ok(BodyLookup::Cached(_)) = self.lookup_body(id, bodies_dir) {
            return WarmTarget::AlreadyWarm;
        }
        let row: Option<(Option<String>, Option<i64>)> = self
            .db
            .conn()
            .query_row(
                "SELECT f.remote_id, m.provider_uid \
                 FROM messages m JOIN folders f ON f.id = m.folder_id \
                 WHERE m.id = ?1",
                params![id.as_str()],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get(1)?)),
            )
            .optional()
            .ok()
            .flatten();
        let Some((folder, uid)) = row else {
            return WarmTarget::Unfetchable;
        };
        // Same two refusals `open_body` makes, and for the same reason: a
        // missing `remote_id` or `provider_uid` would have this build a
        // request out of an identifier nobody ever got from the server.
        let Some(folder) = folder.filter(|s| !s.is_empty()) else {
            return WarmTarget::Unfetchable;
        };
        match uid.and_then(|v| u32::try_from(v).ok()) {
            Some(uid) => WarmTarget::Fetch { folder, uid },
            None => WarmTarget::Unfetchable,
        }
    }
}

enum WarmTarget {
    Fetch { folder: String, uid: u32 },
    AlreadyWarm,
    Unfetchable,
}

/// Maps [`feathermail_sync::SyncError`] to [`CoreError`]. The `String`
/// inside `Session`/`Store` is always a connection/session/store
/// diagnostic (built by `feathermail_providers::sync_session::map_err` from
/// `ConnectError`'s own `message`/`details` fields, or by a `SyncStore`
/// implementer describing its own failure) -- never a byte of the fetched
/// message itself, so surfacing it via `with_details` does not violate D14.
/// `Auth` (T-091) carries no message at all -- see that variant's own doc
/// comment -- so it maps to a fixed, human `AuthRequired` message instead,
/// the same fixed-text shape `ConnectError::Auth`/`ApplyError::Auth`
/// already use elsewhere in this crate for the identical condition.
fn sync_err(e: feathermail_sync::SyncError) -> CoreError {
    match e {
        feathermail_sync::SyncError::Session(msg) => CoreError::new(
            ErrorCode::NetworkUnavailable,
            "Couldn't fetch that message's body from the server.",
        )
        .with_details(msg),
        feathermail_sync::SyncError::Store(msg) => CoreError::new(
            ErrorCode::Conflict,
            "Couldn't fetch that message's body from the server.",
        )
        .with_details(msg),
        feathermail_sync::SyncError::Auth => CoreError::new(
            ErrorCode::AuthRequired,
            ErrorCode::AuthRequired.default_message(),
        ),
    }
}

/// Persist attachment metadata together with the body-cache pointer. The
/// parser already owns the MIME tree; keeping this conversion here means no
/// UI/MCP caller ever has to re-parse a body file or invent an IMAP section
/// number itself.
fn record_attachment_metadata(
    tx: &rusqlite::Transaction<'_>,
    message_id: &MessageId,
    account_id: &str,
    thread_id: &str,
    raw_message: &[u8],
) -> Result<(), CoreError> {
    let parsed = feathermail_html::parse_message(raw_message, true);
    tx.execute(
        "DELETE FROM attachments WHERE message_id = ?1",
        params![message_id.as_str()],
    )
    .map_err(sql_err)?;

    for (index, attachment) in parsed.attachments.iter().enumerate() {
        let filename = attachment
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Attachment {}", index + 1));
        let size_bytes = i64::try_from(attachment.size_bytes).unwrap_or(i64::MAX);
        let encoding = match attachment.transfer_encoding {
            feathermail_html::AttachmentTransferEncoding::Base64 => AttachmentEncoding::Base64,
            feathermail_html::AttachmentTransferEncoding::QuotedPrintable => {
                AttachmentEncoding::QuotedPrintable
            }
            feathermail_html::AttachmentTransferEncoding::Identity => AttachmentEncoding::Identity,
            feathermail_html::AttachmentTransferEncoding::Unsupported => {
                AttachmentEncoding::Unsupported
            }
        };
        // The ordinal is deterministic for the immutable RFC822 source, so
        // a later reparse refers to the same local attachment id. It is not
        // derived from a filename (message content) and never becomes a file
        // path.
        let attachment_id = format!("attachment:{}:{index}", message_id.as_str());
        tx.execute(
            "INSERT INTO attachments
                (id, account_id, message_id, filename, mime, size_bytes,
                 content_id, part_path, transfer_encoding)
             SELECT ?1, account_id, id, ?2, ?3, ?4, ?5, ?6, ?7
             FROM messages WHERE id = ?8",
            params![
                attachment_id,
                filename,
                attachment.content_type,
                size_bytes,
                attachment.content_id,
                attachment.section,
                encoding.as_str(),
                message_id.as_str(),
            ],
        )
        .map_err(sql_err)?;
    }
    let has_attachments = i64::from(!parsed.attachments.is_empty());
    tx.execute(
        "UPDATE messages SET has_attachment = ?1 WHERE id = ?2",
        params![has_attachments, message_id.as_str()],
    )
    .map_err(sql_err)?;
    tx.execute(
        Core::UPDATE_THREAD_HAS_ATTACHMENT_SQL,
        params![account_id, thread_id],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// A failure to *read* from SQLite, told apart from a failure to write.
///
/// [`crate::store::sql_err`] labels every `rusqlite::Error` it is handed
/// "Couldn't save that change." That is simply untrue on a read path, and
/// it is the text an owner-reported "cannot open this letter" was finally
/// traced back to -- the error the reader saw named an operation the code
/// had not attempted.
fn read_err(e: rusqlite::Error) -> CoreError {
    CoreError::new(
        ErrorCode::Conflict,
        "Couldn't read that message from the local cache.",
    )
    .with_details(e.to_string())
}

fn io_err(what: &str, e: io::Error) -> CoreError {
    // D14: never put message content in error text -- only the operation
    // and the I/O error kind ever land here, never a path derived from
    // subject/sender text or any byte of the body itself.
    CoreError::new(ErrorCode::Conflict, "Couldn't save that message's body.")
        .with_details(format!("{what}: {}", e.kind()))
}

/// Sharded, filesystem-safe relative path for `id`'s cache file:
/// `<2 hex chars of a hash>/<sanitized id>.body`. The shard keeps a
/// 10k-message profile from putting 10k files in one directory; the
/// sanitized id (rather than just the hash) keeps the file recognizable
/// when poking around the cache by hand.
fn body_rel_path(id: &MessageId) -> PathBuf {
    let hash = fnv1a_hex(id.as_str());
    let mut safe: String = id
        .as_str()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // `safe` is ASCII by construction, so a byte truncation can never split
    // a character. The cap keeps the whole file name inside the 255-byte
    // limit every filesystem we care about enforces, whatever a provider
    // put in the id.
    safe.truncate(MAX_SAFE_ID_CHARS);
    // The full hash is part of the name, not just the shard directory:
    // `safe` alone is *not* injective (every separator in a message id --
    // `:` in `msg:<account>:<folder>:<uid>`, and anything a folder slug
    // carried over from a server mailbox name -- collapses to `_`), so ids
    // like `...work.2024:5` and `...work:2024:5` sanitize to the same
    // string. Two colliding ids would then share a cache file whenever
    // their shard bytes also matched: one message would render another
    // message's body, and both rows would count the same bytes toward the
    // cache limit. With the hash in the name, a collision needs a genuine
    // 64-bit FNV-1a collision, not a punctuation coincidence.
    Path::new(&hash[..2]).join(format!("{safe}-{hash}.body"))
}

/// Deterministic, sharded and filename-independent cache path for an
/// incoming attachment. A server-provided filename is never used as a path.
pub fn attachment_rel_path(id: &crate::model::AttachmentId) -> PathBuf {
    let hash = fnv1a_hex(id.as_str());
    Path::new(&hash[..2]).join(format!("{hash}.attachment"))
}

fn fnv1a_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Write `contents` to `path` atomically: a temp file in the same
/// directory (so the rename below is same-filesystem, hence atomic), fully
/// written and fsync'd, then renamed onto `path`. Nothing ever observes a
/// partially-written `path`: either the whole rename happens after the
/// whole write succeeded, or `path` is untouched and only the `.tmp`
/// sibling (never looked at by [`Core::lookup_body`], since nothing in
/// `messages.body_path` ever points at it) holds whatever partial bytes
/// got written before the failure.
fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = tmp_path_for(path);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn tmp_path_for(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seeds just enough of `accounts` / `folders` / `threads` / `messages`
    /// for one message row to exist, satisfying the foreign keys `messages`
    /// carries. Mirrors `store::tests::seed_full_account`'s minimal insert
    /// style (that helper lives in a module this task isn't allowed to
    /// touch, so this is a small local copy, not a shared import).
    fn seed_message(core: &Core, msg_id: &str, date: i64) {
        let acc = "acc1";
        let conn = core.db.conn();
        conn.execute(
            "INSERT OR IGNORE INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, 'a@example.com', 'generic', 'synced', 'recent', 0, 0)",
            params![acc],
        )
        .unwrap();
        let inbox = format!("{acc}:inbox");
        conn.execute(
            "INSERT OR IGNORE INTO folders (id, account_id, name, kind) VALUES (?1, ?2, 'Inbox', 'inbox')",
            params![inbox, acc],
        )
        .unwrap();
        let thread = format!("{acc}:t:{msg_id}");
        conn.execute(
            "INSERT OR IGNORE INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
             VALUES (?1, ?2, ?3, 'Hello', 'Hi', ?4, 0)",
            params![thread, acc, inbox, date],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, thread_id, folder_id, date)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![msg_id, acc, thread, inbox, date],
        )
        .unwrap();
    }

    fn core_with_message(msg_id: &str, date: i64) -> Core {
        let core = Core::memory().unwrap();
        seed_message(&core, msg_id, date);
        core
    }

    #[test]
    fn not_cached_is_distinct_from_an_empty_body() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        let id = MessageId("m1".into());

        assert_eq!(
            core.lookup_body(&id, dir.path()).unwrap(),
            BodyLookup::NotCached
        );

        core.store_body(&id, dir.path(), b"").unwrap();
        assert_eq!(
            core.lookup_body(&id, dir.path()).unwrap(),
            BodyLookup::Cached(Vec::new())
        );
    }

    /// T-048's "body arrived later" half, exercised through the real
    /// production call site (`Core::store_body`), not through a test
    /// helper that enqueues by hand. `seed_message` deliberately does not
    /// touch `fts_pending`, so any row that shows up there after
    /// `store_body` came from `store_body` itself.
    #[test]
    fn storing_a_body_queues_the_message_for_background_reindexing() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        let id = MessageId("m1".into());

        let pending_before: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM fts_pending WHERE message_id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending_before, 0, "fixture must not pre-seed fts_pending");

        core.store_body(&id, dir.path(), b"hello world").unwrap();

        let pending_after: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM fts_pending WHERE message_id = 'm1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            pending_after, 1,
            "a cached body must queue its message for reindexing"
        );
    }

    #[test]
    fn unknown_message_id_is_message_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        let id = MessageId("nope".into());
        let err = core.lookup_body(&id, dir.path()).unwrap_err();
        assert_eq!(err.code, ErrorCode::MessageNotFound);

        let mut core = Core::memory().unwrap();
        let err = core.store_body(&id, dir.path(), b"hi").unwrap_err();
        assert_eq!(err.code, ErrorCode::MessageNotFound);
    }

    #[test]
    fn round_trips_crlf_and_non_ascii_bytes_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        let id = MessageId("m1".into());

        let mut contents = b"Subject: hi\r\n\r\nline one\r\nline two\r\n".to_vec();
        contents.extend_from_slice("Привет, мир — © ±\r\n".as_bytes());
        contents.extend_from_slice(&[0u8, 1, 2, 0xff, 0xfe]);

        core.store_body(&id, dir.path(), &contents).unwrap();
        match core.lookup_body(&id, dir.path()).unwrap() {
            BodyLookup::Cached(bytes) => assert_eq!(bytes, contents),
            other => panic!("expected Cached, got {other:?}"),
        }
    }

    #[test]
    fn store_body_persists_bounded_preview_on_message_and_latest_thread() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        let id = MessageId("m1".into());
        let contents = b"Content-Type: text/plain; charset=utf-8\r\n\r\n  Hello\r\n\r\n from the cached body  ";

        core.store_body(&id, dir.path(), contents).unwrap();

        let conn = core.db.conn();
        let message_snippet: String = conn
            .query_row("SELECT snippet FROM messages WHERE id = 'm1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let thread_snippet: String = conn
            .query_row(
                "SELECT snippet FROM threads WHERE id = 'acc1:t:m1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(message_snippet, "Hello from the cached body");
        assert_eq!(thread_snippet, "Hello from the cached body");
    }

    /// T-068: a 10k metadata profile must not turn one cached-body write
    /// into a scan of every thread. The limit is intentionally far looser
    /// than §61's 100 ms UI target: this is a structural regression test
    /// that stays reliable on a busy developer host, while the dedicated
    /// release perf suite asserts the product target separately.
    #[test]
    fn caching_one_body_on_ten_thousand_threads_avoids_a_full_thread_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        {
            let tx = core.db.conn().unchecked_transaction().unwrap();
            tx.execute(
                "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
                 VALUES ('perf', 'Performance', 'perf@example.invalid', 'generic', 'offline', 'recent', 0, 0)",
                [],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO folders (id, account_id, name, kind)
                 VALUES ('perf:inbox', 'perf', 'Inbox', 'inbox')",
                [],
            )
            .unwrap();
            let mut threads = tx
                .prepare(
                    "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date)
                     VALUES (?1, 'perf', 'perf:inbox', 'Synthetic', '', ?2)",
                )
                .unwrap();
            let mut messages = tx
                .prepare(
                    "INSERT INTO messages (id, account_id, thread_id, folder_id, date)
                     VALUES (?1, 'perf', ?2, 'perf:inbox', ?3)",
                )
                .unwrap();
            for n in 0..10_000 {
                let thread_id = format!("perf:t:{n}");
                let message_id = format!("perf:m:{n}");
                threads.execute(params![thread_id, n]).unwrap();
                messages.execute(params![message_id, thread_id, n]).unwrap();
            }
            drop(messages);
            drop(threads);
            tx.commit().unwrap();
        }

        let started = std::time::Instant::now();
        core.store_body(
            &MessageId("perf:m:0".into()),
            dir.path(),
            b"Content-Type: text/plain\r\n\r\nSynthetic cached body",
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "one cached-body write on 10k threads took {elapsed:?}; it must target its thread by primary key"
        );
    }

    /// The two projection writes must use the `threads` primary key. This
    /// checks SQLite's actual plan rather than inferring complexity from a
    /// fast wall-clock sample, so restoring a correlated outer scan makes
    /// the regression fail even on a powerful machine.
    #[test]
    fn cached_body_thread_projections_use_the_primary_key_plan() {
        let core = core_with_message("m1", 0);
        let conn = core.db.conn();
        for (sql, params) in [
            (
                Core::UPDATE_LATEST_THREAD_SNIPPET_SQL,
                rusqlite::params!["fresh preview", "acc1", "acc1:t:m1", "m1"],
            ),
            (
                Core::UPDATE_THREAD_HAS_ATTACHMENT_SQL,
                rusqlite::params!["acc1", "acc1:t:m1"],
            ),
        ] {
            let plan = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map(params, |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert!(
                plan.iter().any(|detail| detail.contains("threads")
                    && detail.contains("sqlite_autoindex_threads_1")),
                "cached-body projection must seek threads by its primary key, plan was {plan:?}"
            );
        }
    }

    /// A second Core handle can rethread a message after a caller has begun
    /// caching its bytes. The shared metadata helper must read ownership
    /// within its own transaction so the new thread, not an earlier owner,
    /// receives the snippet and attachment aggregate.
    #[test]
    fn cached_body_metadata_reloads_a_rethreaded_messages_current_owner_inside_its_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let primary = Core::open(&path).unwrap();
        seed_message(&primary, "m1", 0);
        let secondary = Core::open(&path).unwrap();
        secondary
            .db
            .conn()
            .execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date)
                 VALUES ('acc1:t:new', 'acc1', 'acc1:inbox', 'New', 'old new-thread preview', 1)",
                [],
            )
            .unwrap();
        secondary
            .db
            .conn()
            .execute(
                "UPDATE messages SET thread_id = 'acc1:t:new' WHERE id = 'm1'",
                [],
            )
            .unwrap();

        let tx = primary.db.conn().unchecked_transaction().unwrap();
        Core::store_cached_body_metadata(
            &tx,
            &MessageId("m1".into()),
            "test.body",
            1,
            "fresh current-thread preview",
            b"Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nhello\r\n--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=proof.pdf\r\n\r\naGVsbG8=\r\n--B--\r\n",
        )
        .unwrap();
        tx.commit().unwrap();

        let old: (String, i64) = primary
            .db
            .conn()
            .query_row(
                "SELECT snippet, has_attachment FROM threads WHERE id = 'acc1:t:m1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let current: (String, i64) = primary
            .db
            .conn()
            .query_row(
                "SELECT snippet, has_attachment FROM threads WHERE id = 'acc1:t:new'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(old, ("Hi".into(), 0), "old owner must stay untouched");
        assert_eq!(
            current,
            ("fresh current-thread preview".into(), 1),
            "the transaction must update the message's current owner"
        );
    }

    #[test]
    fn a_stray_tmp_file_never_reads_back_as_a_cached_body() {
        // Simulate the crash point store_body's atomic write is meant to
        // guard against: bytes make it to the `.tmp` sibling but the
        // rename (and therefore the `body_path` update) never happens.
        // This exercises the actual observable contract -- what
        // `lookup_body` returns -- rather than asserting `rename` appears
        // somewhere in the source.
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        let id = MessageId("m1".into());

        let rel = body_rel_path(&id);
        let full = dir.path().join(&rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        let tmp = tmp_path_for(&full);
        fs::write(&tmp, b"only half of the real content").unwrap();

        assert!(!full.exists(), "canonical path must not exist pre-rename");
        assert_eq!(
            core.lookup_body(&id, dir.path()).unwrap(),
            BodyLookup::NotCached,
            "a body_path column that was never set must not be satisfied by a stray tmp file"
        );
    }

    #[test]
    fn a_full_second_write_fully_replaces_a_stale_tmp_sibling() {
        // The tmp file is reused (same name) across writes; make sure a
        // shorter previous tmp payload's leftovers can't survive a later,
        // longer write (File::create truncates, but this pins the
        // behavior rather than trusting that).
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        let id = MessageId("m1".into());

        let rel = body_rel_path(&id);
        let full = dir.path().join(&rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(
            tmp_path_for(&full),
            b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX",
        )
        .unwrap();

        core.store_body(&id, dir.path(), b"short").unwrap();
        match core.lookup_body(&id, dir.path()).unwrap() {
            BodyLookup::Cached(bytes) => assert_eq!(bytes, b"short"),
            other => panic!("expected Cached, got {other:?}"),
        }
    }

    /// T-134. An HTML letter whose images carry tracking parameters was
    /// stored with "&&&&&&&&" in front of its text. The parser is fixed;
    /// this is the pass that repairs what the broken one already wrote.
    #[test]
    fn a_queued_snippet_is_recomputed_from_the_cached_body() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        let id = MessageId("m1".into());
        let raw = b"Content-Type: text/html\r\n\r\n\
                    <img src=\"https://t.example/x?a=1&b=2&c=3\"><p>Real text</p>";
        core.store_body(&id, dir.path(), raw).unwrap();
        // What the broken parser left behind, put back by hand.
        core.db
            .conn()
            .execute(
                "UPDATE messages SET snippet = '&&&& Real text' WHERE id = 'm1';",
                [],
            )
            .unwrap();
        core.db
            .conn()
            .execute("INSERT INTO snippet_repairs (message_id) VALUES ('m1')", [])
            .unwrap();

        let result = core
            .repair_snippet_batch(dir.path(), DEFAULT_SNIPPET_REPAIR_BATCH)
            .unwrap();
        assert_eq!(result.repaired, 1);
        assert_eq!(result.remaining, 0, "a handled row leaves the queue");

        let (message_snippet, thread_snippet): (String, String) = core
            .db
            .conn()
            .query_row(
                "SELECT m.snippet, t.snippet FROM messages m
                 JOIN threads t ON t.id = m.thread_id WHERE m.id = 'm1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(
            !message_snippet.contains("&&"),
            "the ampersand run is what this exists to remove: {message_snippet}"
        );
        assert!(message_snippet.contains("Real text"));
        assert_eq!(
            thread_snippet, message_snippet,
            "the list shows the thread projection, so it has to be repaired too"
        );
    }

    /// The queue has to empty even when nothing needs changing, or the
    /// worker would re-read the same bodies for the rest of the profile's
    /// life. Two ways a row can be a no-op: its `&&` is genuine, or its
    /// cached body has since been evicted.
    #[test]
    fn a_row_that_needs_no_repair_still_leaves_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        seed_message(&core, "m2", 1);
        let id = MessageId("m1".into());
        core.store_body(&id, dir.path(), b"Tom && Jerry").unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO snippet_repairs (message_id) VALUES ('m1'), ('m2')",
                [],
            )
            .unwrap();

        let result = core
            .repair_snippet_batch(dir.path(), DEFAULT_SNIPPET_REPAIR_BATCH)
            .unwrap();
        assert_eq!(result.repaired, 0, "neither row needed a new snippet");
        assert_eq!(result.remaining, 0, "and neither row is looked at again");
        let kept: String = core
            .db
            .conn()
            .query_row("SELECT snippet FROM messages WHERE id = 'm1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kept, "Tom && Jerry", "a genuine ampersand is not damage");
    }

    #[test]
    fn dangling_body_path_self_heals_to_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        let id = MessageId("m1".into());
        core.store_body(&id, dir.path(), b"hello").unwrap();

        // Simulate the file being lost out from under the DB pointer.
        let rel = body_rel_path(&id);
        fs::remove_file(dir.path().join(&rel)).unwrap();

        assert_eq!(
            core.lookup_body(&id, dir.path()).unwrap(),
            BodyLookup::NotCached
        );

        // Self-heal must actually clear the row, not just the return value
        // of this one call -- otherwise the cache-size SUM() would go on
        // overcounting a body that no longer exists on disk forever.
        let (body_path, body_bytes): (Option<String>, Option<i64>) = core
            .db
            .conn()
            .query_row(
                "SELECT body_path, body_bytes FROM messages WHERE id = 'm1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(body_path, None);
        assert_eq!(body_bytes, None);
    }

    /// T-111: seed one attachment row on `msg_id` and put `bytes` on disk
    /// where the cache expects them, exactly the way service's
    /// `download_one_attachment` does it. `declared` goes into
    /// `size_bytes` -- the server's number, deliberately allowed to differ
    /// from what is actually written.
    fn seed_attachment(core: &Core, att_id: &str, msg_id: &str, declared: i64) {
        core.db
            .conn()
            .execute(
                "INSERT INTO attachments (id, account_id, message_id, filename, mime, size_bytes, part_path)
                 VALUES (?1, 'acc1', ?2, 'file.bin', 'application/octet-stream', ?3, '2')",
                params![att_id, msg_id, declared],
            )
            .unwrap();
    }

    fn cache_attachment(core: &mut Core, dir: &Path, att_id: &str, bytes: &[u8]) {
        let id = crate::model::AttachmentId(att_id.to_string());
        let rel = attachment_rel_path(&id);
        let full = dir.join(&rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, bytes).unwrap();
        core.mark_attachment_cached(&crate::model::AccountId("acc1".into()), &id, &rel, dir)
            .unwrap();
    }

    fn cached_attachments(core: &Core) -> Vec<String> {
        let conn = core.db.conn();
        let mut stmt = conn
            .prepare("SELECT id FROM attachments WHERE cache_path IS NOT NULL ORDER BY id")
            .unwrap();
        let out = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        out
    }

    /// T-111: before this the attachment cache had no budget at all -- only
    /// bodies were swept -- so downloading files grew the profile without a
    /// ceiling.
    ///
    /// Mutation: drop the `enforce_attachment_cache_limit_keeping` call from
    /// `mark_attachment_cached` -> all three files stay and this fails.
    #[test]
    fn attachment_cache_over_budget_evicts_oldest_mail_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_message(&core, "old", 1);
        seed_message(&core, "mid", 2);
        seed_message(&core, "new", 3);
        seed_attachment(&core, "a-old", "old", 0);
        seed_attachment(&core, "a-mid", "mid", 0);
        seed_attachment(&core, "a-new", "new", 0);
        core.patch_settings(0, |s| s.attachment_cache_limit_bytes = 250);

        cache_attachment(&mut core, dir.path(), "a-old", &[b'x'; 100]);
        cache_attachment(&mut core, dir.path(), "a-mid", &[b'x'; 100]);
        // 300 > 250, and the file that just landed is never its own victim,
        // so the attachment on the oldest mail goes.
        cache_attachment(&mut core, dir.path(), "a-new", &[b'x'; 100]);

        assert_eq!(cached_attachments(&core), vec!["a-mid", "a-new"]);
        let gone = dir
            .path()
            .join(attachment_rel_path(&crate::model::AttachmentId(
                "a-old".into(),
            )));
        assert!(
            !gone.exists(),
            "the evicted attachment's file must be removed from disk, not just unlinked in the DB"
        );
    }

    /// T-111: eviction takes the file, never the mail. An attachment the
    /// owner can no longer open offline must still be listed, named, sized
    /// and re-downloadable.
    #[test]
    fn evicting_an_attachment_keeps_its_row_and_the_way_back_to_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_message(&core, "old", 1);
        seed_message(&core, "new", 2);
        seed_attachment(&core, "a-old", "old", 0);
        seed_attachment(&core, "a-new", "new", 0);
        core.patch_settings(0, |s| s.attachment_cache_limit_bytes = 150);

        cache_attachment(&mut core, dir.path(), "a-old", &[b'x'; 100]);
        cache_attachment(&mut core, dir.path(), "a-new", &[b'x'; 100]);

        let (filename, mime, part, cache_path, cache_bytes): (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
        ) = core
            .db
            .conn()
            .query_row(
                "SELECT filename, mime, part_path, cache_path, cache_bytes \
                 FROM attachments WHERE id = 'a-old'",
                [],
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
            .unwrap();
        assert_eq!(filename, "file.bin");
        assert_eq!(mime, "application/octet-stream");
        assert_eq!(part.as_deref(), Some("2"), "the IMAP part it comes from");
        assert_eq!(cache_path, None, "only the cached copy is gone");
        assert_eq!(cache_bytes, None);
    }

    /// T-111: `cache_bytes` is the file that was written, not the server's
    /// `size_bytes` for the encoded part -- those differ by about a third
    /// for base64, and spending the budget on the wire number would evict
    /// early for every attachment in the profile.
    #[test]
    fn cached_attachment_size_comes_from_the_file_not_from_the_server_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_message(&core, "m1", 1);
        seed_attachment(&core, "a1", "m1", 4_000);

        cache_attachment(&mut core, dir.path(), "a1", &[b'x'; 3_000]);

        let cache_bytes: Option<i64> = core
            .db
            .conn()
            .query_row(
                "SELECT cache_bytes FROM attachments WHERE id = 'a1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cache_bytes, Some(3_000));
    }

    /// T-111: a row cached before schema v25 has no `cache_bytes`. It must
    /// still count against the budget -- at `size_bytes`, the only number
    /// this profile has for it -- rather than being invisible and immortal.
    #[test]
    fn an_attachment_cached_before_the_size_column_still_counts_and_can_be_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_message(&core, "old", 1);
        seed_message(&core, "new", 2);
        seed_attachment(&core, "a-old", "old", 200);
        seed_attachment(&core, "a-new", "new", 0);

        // Exactly what a v24 profile looks like after the migration: a
        // pointer, a file, and no size.
        let old = crate::model::AttachmentId("a-old".into());
        let rel = attachment_rel_path(&old);
        let full = dir.path().join(&rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, [b'x'; 200]).unwrap();
        core.db
            .conn()
            .execute(
                "UPDATE attachments SET cache_path = ?1, cache_bytes = NULL WHERE id = 'a-old'",
                params![rel.to_string_lossy()],
            )
            .unwrap();

        core.patch_settings(0, |s| s.attachment_cache_limit_bytes = 150);
        cache_attachment(&mut core, dir.path(), "a-new", &[b'x'; 100]);

        assert_eq!(
            cached_attachments(&core),
            vec!["a-new"],
            "the sizeless row counted at its declared 200 bytes and went first"
        );
        assert!(!full.exists());
    }

    /// T-111: the file that triggered the sweep is never its own victim --
    /// the same rule `store_body` follows, and for the same reason: the
    /// owner asked for *this* attachment, and evicting it would send the
    /// download straight back to the network.
    #[test]
    fn the_attachment_just_downloaded_survives_a_sweep_it_triggered() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_message(&core, "m1", 1);
        seed_attachment(&core, "a1", "m1", 0);
        core.patch_settings(0, |s| s.attachment_cache_limit_bytes = 10);

        cache_attachment(&mut core, dir.path(), "a1", &[b'x'; 100]);

        assert_eq!(cached_attachments(&core), vec!["a1"]);
    }

    #[test]
    fn cache_total_is_computed_from_the_body_bytes_column_not_from_stat() {
        // F1: enforce_body_cache_limit must decide purely from
        // SUM(body_bytes), never from fs::metadata. Prove it by making the
        // column and the real on-disk file disagree wildly, then showing
        // the eviction decision follows the column, not the disk.
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        let id = MessageId("m1".into());
        core.store_body(&id, dir.path(), &[b'x'; 100]).unwrap();

        // Overwrite the real file to be far larger than what the column
        // says, without touching body_bytes -- simulates the column having
        // drifted from disk (see the module doc's note on this).
        let rel = body_rel_path(&id);
        fs::write(dir.path().join(&rel), vec![b'y'; 10_000_000]).unwrap();

        // A limit that sits strictly between the column's total (100) and
        // the real file size (10_000_000): if the sweep were stat()-based
        // it would see 10MB > limit and evict; column-based, it sees 100 <=
        // limit and must not touch anything.
        let evicted = core.enforce_body_cache_limit(dir.path(), 1000).unwrap();
        assert_eq!(evicted, 0, "column total (100) is under the limit (1000)");
        assert_eq!(
            core.lookup_body(&id, dir.path()).unwrap(),
            BodyLookup::Cached(vec![b'y'; 10_000_000]),
            "file must be untouched -- no eviction should have run"
        );
    }

    #[test]
    fn eviction_drops_oldest_dated_bodies_first_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_message(&core, "old", 1);
        seed_message(&core, "mid", 2);
        seed_message(&core, "new", 3);

        let body = vec![b'x'; 100];
        core.patch_settings(0, |s| s.cache_limit_bytes = 250);

        core.store_body(&MessageId("old".into()), dir.path(), &body)
            .unwrap();
        core.store_body(&MessageId("mid".into()), dir.path(), &body)
            .unwrap();
        // Total would be 300 > 250 -- "old" (earliest date) must go.
        core.store_body(&MessageId("new".into()), dir.path(), &body)
            .unwrap();

        assert_eq!(
            core.lookup_body(&MessageId("old".into()), dir.path())
                .unwrap(),
            BodyLookup::NotCached,
            "oldest-dated body should have been evicted"
        );
        assert_eq!(
            core.lookup_body(&MessageId("mid".into()), dir.path())
                .unwrap(),
            BodyLookup::Cached(body.clone())
        );
        assert_eq!(
            core.lookup_body(&MessageId("new".into()), dir.path())
                .unwrap(),
            BodyLookup::Cached(body)
        );

        let rel = body_rel_path(&MessageId("old".into()));
        assert!(
            !dir.path().join(rel).exists(),
            "evicted body's file must actually be removed from disk, not just unlinked in the DB"
        );
    }

    /// T-102 (владелец: «то что уже открывали нужно кешировать на пару
    /// дней»): the sweep used to order by `messages.date` alone, so the mail
    /// the owner opened a minute ago -- an old thread -- was the first thing
    /// thrown out, and reopening it went back to the network.
    ///
    /// Mutation: drop the `body_read_at` key from the ORDER BY -> "old" is
    /// evicted again and this fails.
    #[test]
    fn a_body_the_owner_opened_outlives_newer_mail_nobody_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        core.set_now(1_000_000);
        seed_message(&core, "old", 1);
        seed_message(&core, "mid", 2);
        seed_message(&core, "new", 3);

        let body = vec![b'x'; 100];
        core.patch_settings(0, |s| s.cache_limit_bytes = 250);
        core.store_body(&MessageId("old".into()), dir.path(), &body)
            .unwrap();
        core.store_body(&MessageId("mid".into()), dir.path(), &body)
            .unwrap();

        // The owner opens the oldest mail. That read is the whole difference
        // between this test and `eviction_drops_oldest_dated_bodies_first`.
        assert_eq!(
            core.lookup_body(&MessageId("old".into()), dir.path())
                .unwrap(),
            BodyLookup::Cached(body.clone())
        );

        // A third body arrives (a warm-up, say) and something has to go.
        core.store_body(&MessageId("new".into()), dir.path(), &body)
            .unwrap();

        assert_eq!(
            core.lookup_body(&MessageId("old".into()), dir.path())
                .unwrap(),
            BodyLookup::Cached(body.clone()),
            "the mail the owner just opened must survive the sweep"
        );
        assert_eq!(
            core.lookup_body(&MessageId("mid".into()), dir.path())
                .unwrap(),
            BodyLookup::NotCached,
            "the oldest body nobody opened is the one that goes"
        );
    }

    /// A read older than the grace window is no longer a shield: "a couple of
    /// days" is a promise with an end, not a permanent pin.
    #[test]
    fn a_read_from_last_week_no_longer_protects_a_body() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_message(&core, "old", 1);
        seed_message(&core, "mid", 2);
        seed_message(&core, "new", 3);

        let body = vec![b'x'; 100];
        core.patch_settings(0, |s| s.cache_limit_bytes = 250);
        core.set_now(1_000_000);
        core.store_body(&MessageId("old".into()), dir.path(), &body)
            .unwrap();
        core.store_body(&MessageId("mid".into()), dir.path(), &body)
            .unwrap();
        assert!(matches!(
            core.lookup_body(&MessageId("old".into()), dir.path())
                .unwrap(),
            BodyLookup::Cached(_)
        ));

        // A week later, that read is out of the window.
        core.set_now(1_000_000 + 7 * 24 * 60 * 60);
        core.store_body(&MessageId("new".into()), dir.path(), &body)
            .unwrap();

        assert_eq!(
            core.lookup_body(&MessageId("old".into()), dir.path())
                .unwrap(),
            BodyLookup::NotCached,
            "past the grace window the plain oldest-first order is back"
        );
    }

    /// Protection orders the queue; it does not make the cache unbounded. If
    /// every cached body was opened this minute, the budget still wins -- and
    /// inside the protected group the oldest mail is still first out.
    #[test]
    fn an_all_protected_cache_still_gives_way_to_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        core.set_now(1_000_000);
        seed_message(&core, "old", 1);
        seed_message(&core, "new", 2);

        let body = vec![b'x'; 100];
        core.patch_settings(0, |s| s.cache_limit_bytes = 150);
        core.store_body(&MessageId("old".into()), dir.path(), &body)
            .unwrap();
        assert!(matches!(
            core.lookup_body(&MessageId("old".into()), dir.path())
                .unwrap(),
            BodyLookup::Cached(_)
        ));
        core.store_body(&MessageId("new".into()), dir.path(), &body)
            .unwrap();

        assert!(
            matches!(
                core.lookup_body(&MessageId("new".into()), dir.path())
                    .unwrap(),
                BodyLookup::Cached(_)
            ),
            "the body that triggered the sweep is never its own victim -- \
             otherwise the reader refetches what was just written"
        );
        assert_eq!(
            core.lookup_body(&MessageId("old".into()), dir.path())
                .unwrap(),
            BodyLookup::NotCached,
            "two 100-byte bodies do not fit in 150 bytes, protected or not"
        );
    }

    /// The stamp is written by the read itself, not by caching: a warm-up
    /// nobody opened must not outrank mail the owner actually read.
    #[test]
    fn caching_a_body_does_not_count_as_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        core.set_now(4_242);
        seed_message(&core, "m1", 1);
        core.store_body(&MessageId("m1".into()), dir.path(), b"body")
            .unwrap();

        let after_store: Option<i64> = core
            .db
            .conn()
            .query_row("SELECT body_read_at FROM messages WHERE id='m1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(after_store, None, "a prefetch is not a read");

        core.lookup_body(&MessageId("m1".into()), dir.path())
            .unwrap();
        let after_read: Option<i64> = core
            .db
            .conn()
            .query_row("SELECT body_read_at FROM messages WHERE id='m1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(after_read, Some(4_242));
    }

    /// A busy writer must not break, or noticeably delay, opening a letter
    /// whose body is already on disk.
    ///
    /// This is the owner's "Something went wrong reading this message from
    /// the local cache", reduced: the 100-message warm-up holds SQLite's
    /// single writer for a few hundred milliseconds at a time, and the read
    /// path used to take a write of its own (`body_read_at`) and propagate
    /// its failure. The read itself never needed that write to succeed.
    #[test]
    fn a_busy_writer_neither_breaks_nor_stalls_a_cache_hit() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mail.db");
        let bodies = dir.path().join("bodies");
        std::fs::create_dir_all(&bodies).unwrap();

        let mut core = Core::open(&db_path).unwrap();
        seed_message(&core, "m1", 1);
        let id = MessageId("m1".into());
        core.store_body(&id, &bodies, b"body").unwrap();

        // Exactly what the warm-up does: another connection to the same
        // file, holding the write lock while this read runs.
        let blocker = rusqlite::Connection::open(&db_path).unwrap();
        blocker
            .execute_batch("BEGIN IMMEDIATE; UPDATE messages SET unread = unread;")
            .unwrap();

        let started = std::time::Instant::now();
        let got = core.lookup_body(&id, &bodies);
        let waited = started.elapsed();

        blocker.execute_batch("ROLLBACK").unwrap();

        assert_eq!(
            got.unwrap(),
            BodyLookup::Cached(b"body".to_vec()),
            "a cache hit must survive a busy writer -- the bytes were already read"
        );
        assert!(
            waited < Duration::from_secs(1),
            "the read-stamp must try for the lock, not queue for it: waited {waited:?}"
        );
    }

    #[test]
    fn under_the_limit_nothing_is_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m1", 0);
        core.patch_settings(0, |s| s.cache_limit_bytes = 1_000_000);
        core.store_body(&MessageId("m1".into()), dir.path(), b"tiny")
            .unwrap();
        assert_eq!(
            core.lookup_body(&MessageId("m1".into()), dir.path())
                .unwrap(),
            BodyLookup::Cached(b"tiny".to_vec())
        );
    }

    /// Two message ids that differ only in punctuation sanitize to the
    /// same readable stem (`:` and `.` both become `_`). Before the hash
    /// was part of the file name they could share a cache file whenever
    /// their shard bytes also matched -- one message rendering another
    /// message's body. Asserted on the bytes actually read back, not on
    /// the path string.
    #[test]
    fn ids_differing_only_in_punctuation_never_share_a_cache_file() {
        // `work.233` / `work:233` both sanitize to `msg_acc1_work_233_5`
        // *and* land in the same `2d` shard directory -- picked deliberately
        // so this test fails on the stem alone, not on shard luck.
        let a = MessageId("msg:acc1:work.233:5".into());
        let b = MessageId("msg:acc1:work:233:5".into());
        assert_ne!(
            body_rel_path(&a),
            body_rel_path(&b),
            "distinct message ids must never map to the same cache file"
        );

        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_message(&core, a.as_str(), 0);
        seed_message(&core, b.as_str(), 0);
        core.store_body(&a, dir.path(), b"body of A").unwrap();
        core.store_body(&b, dir.path(), b"body of B").unwrap();

        assert_eq!(
            core.lookup_body(&a, dir.path()).unwrap(),
            BodyLookup::Cached(b"body of A".to_vec())
        );
        assert_eq!(
            core.lookup_body(&b, dir.path()).unwrap(),
            BodyLookup::Cached(b"body of B".to_vec())
        );
    }

    /// A provider is free to hand us a very long id; the cache file name
    /// must still fit what a filesystem will accept, and staying unique
    /// must not depend on the part that got truncated.
    #[test]
    fn a_very_long_message_id_still_produces_a_usable_unique_file_name() {
        let long_a = MessageId(format!("msg:acc1:{}:5", "x".repeat(400)));
        let long_b = MessageId(format!("msg:acc1:{}:6", "x".repeat(400)));
        for id in [&long_a, &long_b] {
            let rel = body_rel_path(id);
            let name = rel.file_name().unwrap().to_string_lossy();
            assert!(
                name.len() <= 255,
                "cache file name must fit the filesystem limit, got {} bytes",
                name.len()
            );
        }
        assert_ne!(body_rel_path(&long_a), body_rel_path(&long_b));

        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_message(&core, long_a.as_str(), 0);
        core.store_body(&long_a, dir.path(), b"long id body")
            .unwrap();
        assert_eq!(
            core.lookup_body(&long_a, dir.path()).unwrap(),
            BodyLookup::Cached(b"long id body".to_vec())
        );
    }

    /// T-028: opening a cached body is a local disk read -- `lookup_body`
    /// takes no `MailboxSession`, no provider, no network. If this ever
    /// started needing a live IMAP session, the signature itself would
    /// change and this test would not compile, which is the point.
    #[test]
    fn a_cached_body_opens_offline_from_the_on_disk_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m-cached", 0);
        let id = MessageId("m-cached".into());
        let contents = b"From: a@example.com\r\nSubject: cached\r\n\r\nhello from disk\r\n";
        core.store_body(&id, dir.path(), contents).unwrap();
        match core.lookup_body(&id, dir.path()).unwrap() {
            BodyLookup::Cached(bytes) => assert_eq!(bytes, contents),
            other => panic!("expected Cached for an on-disk body, got {other:?}"),
        }
    }

    #[test]
    fn caching_a_body_persists_downloadable_attachment_metadata_in_core() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m-attachments", 0);
        let id = MessageId("m-attachments".into());
        let raw = b"Content-Type: multipart/mixed; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nhello\r\n--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=report.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--B--\r\n";
        core.store_body(&id, dir.path(), raw).unwrap();

        let attachments = core
            .list_attachments(&crate::model::AccountId("acc1".into()), &id)
            .unwrap();
        assert_eq!(attachments.len(), 1);
        let attachment = &attachments[0];
        assert_eq!(attachment.filename, "report.pdf");
        assert_eq!(attachment.mime, "application/pdf");
        assert_eq!(attachment.size_bytes, 5);
        assert_eq!(attachment.part_path.as_deref(), Some("2"));
        assert_eq!(attachment.transfer_encoding, AttachmentEncoding::Base64);
        assert!(attachment.cache_path.is_none());
        assert_eq!(
            core.get_attachment(&crate::model::AccountId("acc1".into()), &attachment.id)
                .unwrap(),
            attachment.clone(),
            "a single attachment must remain available through Core without a caller reading SQLite"
        );
        let has_attachment: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT has_attachment FROM messages WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_attachment, 1);
    }

    /// T-028: warm-cache preview budget. Isolates `Core::lookup_body`
    /// against a body already on disk (the GTK-thread path is
    /// `spawn_body_lookup`, already covered by its own D11 test in
    /// `crates/app`). Best of several samples, same reasoning as
    /// `search_over_ten_thousand_messages_stays_under_the_budget`:
    /// contention can only slow a run down.
    #[test]
    fn lookup_body_on_a_warm_cache_stays_under_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = core_with_message("m-warm", 0);
        let id = MessageId("m-warm".into());
        // A typical short mail, not an empty stub -- the budget is about
        // opening what the user actually cached.
        let contents = {
            let mut v = b"From: a@example.com\r\nTo: me@example.com\r\nSubject: warm cache\r\n\r\n"
                .to_vec();
            v.extend(std::iter::repeat_n(b'x', 32 * 1024));
            v
        };
        core.store_body(&id, dir.path(), &contents).unwrap();

        match core.lookup_body(&id, dir.path()).unwrap() {
            BodyLookup::Cached(bytes) => assert_eq!(bytes.len(), contents.len()),
            other => panic!("warmup must hit the cache, got {other:?}"),
        }

        let mut best = std::time::Duration::from_secs(3600);
        for _ in 0..7 {
            let start = std::time::Instant::now();
            let got = core.lookup_body(&id, dir.path()).unwrap();
            let elapsed = start.elapsed();
            match got {
                BodyLookup::Cached(bytes) => assert_eq!(bytes.len(), contents.len()),
                other => panic!("warm lookup must stay Cached, got {other:?}"),
            }
            best = best.min(elapsed);
        }
        assert!(
            best < std::time::Duration::from_millis(100),
            "warm-cache lookup_body took {best:?} (best of 7 samples), budget is 100ms"
        );
    }

    #[test]
    fn default_bodies_dir_is_a_sibling_of_mail_db() {
        let dir = default_bodies_dir();
        assert_eq!(dir.file_name().unwrap(), BODIES_DIR_NAME);
        let expected_parent = feathermail_db::default_db_path()
            .parent()
            .unwrap()
            .to_path_buf();
        assert_eq!(dir.parent().unwrap(), expected_parent);
    }

    /// A hand-rolled `MailboxSession` standing in for IMAP, same pattern as
    /// `sync_store::tests::FakeSession` -- logs every call it receives so
    /// tests can assert on ordering/arguments, not just the return value.
    struct FakeSession {
        bodies: std::collections::HashMap<(String, u32), Vec<u8>>,
        call_log: Vec<String>,
    }

    impl feathermail_sync::MailboxSession for FakeSession {
        fn select(
            &mut self,
            folder: &str,
        ) -> Result<feathermail_sync::MailboxSnapshot, feathermail_sync::SyncError> {
            self.call_log.push(format!("select:{folder}"));
            Ok(feathermail_sync::MailboxSnapshot {
                uidvalidity: 1,
                uidnext: 1,
                exists: 0,
                highest_modseq: None,
            })
        }
        fn uid_fetch_headers(
            &mut self,
            _folder: &str,
            _range: feathermail_sync::UidRange,
        ) -> Result<Vec<feathermail_sync::HeaderMeta>, feathermail_sync::SyncError> {
            Ok(Vec::new())
        }
        fn uid_fetch_flags_changed_since(
            &mut self,
            _folder: &str,
            _range: feathermail_sync::UidRange,
            _modseq: u64,
        ) -> Result<Vec<feathermail_sync::HeaderMeta>, feathermail_sync::SyncError> {
            Ok(Vec::new())
        }
        fn list_folders(&mut self) -> Result<Vec<String>, feathermail_sync::SyncError> {
            Ok(Vec::new())
        }
        fn fetch_body(
            &mut self,
            folder: &str,
            uid: u32,
        ) -> Result<Vec<u8>, feathermail_sync::SyncError> {
            self.call_log.push(format!("fetch_body:{folder}:{uid}"));
            self.bodies
                .get(&(folder.to_string(), uid))
                .cloned()
                .ok_or_else(|| feathermail_sync::SyncError::Session(format!("no such uid {uid}")))
        }
        /// Logs the whole set as one call, the way a real batched
        /// `UID FETCH` is one command -- that is the property the warm-up
        /// tests are about. A UID with nothing behind it is simply absent
        /// from the answer, exactly as a server omits an expunged one.
        fn fetch_bodies(
            &mut self,
            folder: &str,
            uids: &[u32],
        ) -> Result<Vec<(u32, Vec<u8>)>, feathermail_sync::SyncError> {
            let list = uids
                .iter()
                .map(|u| u.to_string())
                .collect::<Vec<_>>()
                .join(",");
            self.call_log.push(format!("fetch_bodies:{folder}:{list}"));
            Ok(uids
                .iter()
                .filter_map(|uid| {
                    self.bodies
                        .get(&(folder.to_string(), *uid))
                        .cloned()
                        .map(|bytes| (*uid, bytes))
                })
                .collect())
        }
    }

    /// Seeds a message the same way [`seed_message`] does, but also gives
    /// its folder a `remote_id` and the message a `provider_uid`, since
    /// [`Core::open_body`] needs both to know what to ask the session for.
    fn seed_synced_message(core: &Core, msg_id: &str, remote_folder: &str, uid: u32) {
        seed_message(core, msg_id, 0);
        let conn = core.db.conn();
        conn.execute(
            "UPDATE folders SET remote_id = ?1 WHERE id = (SELECT folder_id FROM messages WHERE id = ?2)",
            params![remote_folder, msg_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE messages SET provider_uid = ?1 WHERE id = ?2",
            params![uid, msg_id],
        )
        .unwrap();
    }

    /// T-024 batched: the warm-up asks once per folder, not once per
    /// message, and reports every id honestly.
    ///
    /// On the owner's mailbox a single body cost ~400 ms of round trip
    /// almost regardless of its size, so a hundred-message warm-up spent
    /// forty seconds holding the account's only connection -- which is
    /// the queue a click then had to wait in.
    #[test]
    fn warming_a_set_is_one_fetch_per_folder_and_reports_each_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_synced_message(&core, "in1", "INBOX", 11);
        seed_synced_message(&core, "in2", "INBOX", 12);
        seed_synced_message(&core, "gone", "INBOX", 99);
        seed_message(&core, "unsynced", 5);
        {
            // A genuinely second folder: `seed_message` puts everything in
            // the account's inbox, so grouping cannot be exercised without
            // one that really is somewhere else.
            let conn = core.db.conn();
            conn.execute(
                "INSERT INTO folders (id, account_id, name, kind, remote_id) \
                 VALUES ('acc1:archive', 'acc1', 'Archive', 'archive', 'ARCHIVE')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread) \
                 VALUES ('acc1:t:arch', 'acc1', 'acc1:archive', 'A', 'A', 5, 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (id, account_id, thread_id, folder_id, provider_uid, date, \
                                       sender_name, sender_email, recipients, subject, snippet, \
                                       unread, starred, has_attachment, importance, size_bytes) \
                 VALUES ('arch', 'acc1', 'acc1:t:arch', 'acc1:archive', 21, 5, \
                         'A', 'a@example.com', '', 'A', 'A', 0, 0, 0, 'normal', 10)",
                [],
            )
            .unwrap();
        }

        let mut bodies = std::collections::HashMap::new();
        bodies.insert(("INBOX".to_string(), 11), b"one".to_vec());
        bodies.insert(("INBOX".to_string(), 12), b"two".to_vec());
        bodies.insert(("ARCHIVE".to_string(), 21), b"three".to_vec());
        let mut session = FakeSession {
            bodies,
            call_log: Vec::new(),
        };

        let ids: Vec<MessageId> = ["in1", "in2", "arch", "gone", "unsynced"]
            .into_iter()
            .map(|s| MessageId(s.to_string()))
            .collect();
        let mut got = core.warm_bodies(&mut session, &ids, dir.path());
        got.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));

        let mut want = vec![
            (MessageId("in1".into()), true),
            (MessageId("in2".into()), true),
            (MessageId("arch".into()), true),
            // The server did not return this UID: not warmed, and not a
            // reason to fail the rest.
            (MessageId("gone".into()), false),
            // Never matched to a server UID, so there was nothing to ask
            // for -- refused before the network, like `open_body` does.
            (MessageId("unsynced".into()), false),
        ];
        want.sort_by(|a, b| a.0 .0.cmp(&b.0 .0));
        assert_eq!(got, want);

        assert_eq!(
            session.call_log,
            vec![
                "select:INBOX",
                "fetch_bodies:INBOX:11,12,99",
                "select:ARCHIVE",
                "fetch_bodies:ARCHIVE:21",
            ],
            "one select and one fetch per folder, in the order the ids arrived"
        );

        // What was warmed is on disk, so opening it never touches the
        // session again -- the whole point of warming.
        session.call_log.clear();
        assert_eq!(
            core.lookup_body(&MessageId("in2".into()), dir.path())
                .unwrap(),
            BodyLookup::Cached(b"two".to_vec())
        );
        assert!(session.call_log.is_empty());
    }

    /// T-108: the merged view warms like any other folder.
    ///
    /// The owner's rule: "the merged mailbox should work the same as a
    /// normal one, it just has mail from all the connected accounts."
    /// Warming was the one place that was not true -- it bailed out and
    /// every click there took the cold path.
    #[test]
    fn the_merged_view_warms_across_accounts_and_says_which_one() {
        let core = Core::memory().unwrap();
        seed_synced_message(&core, "a-old", "INBOX", 1);
        seed_synced_message(&core, "a-new", "INBOX", 2);
        {
            let conn = core.db.conn();
            // A second mailbox, its own inbox folder and its own thread.
            conn.execute(
                "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at) \
                 VALUES ('acc2', 'acc2', 'b@example.com', 'generic', 'synced', 'recent', 0, 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO folders (id, account_id, name, kind, remote_id) \
                 VALUES ('acc2:inbox', 'acc2', 'Inbox', 'inbox', 'INBOX')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread) \
                 VALUES ('acc2:t:b1', 'acc2', 'acc2:inbox', 'Hi', 'Hi', 250, 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (id, account_id, thread_id, folder_id, provider_uid, date, \
                                       sender_name, sender_email, recipients, subject, snippet, \
                                       unread, starred, has_attachment, importance, size_bytes) \
                 VALUES ('b1', 'acc2', 'acc2:t:b1', 'acc2:inbox', 7, 250, \
                         'B', 'b@example.com', '', 'Hi', 'Hi', 0, 0, 0, 'normal', 10)",
                [],
            )
            .unwrap();
            conn.execute("UPDATE messages SET date = 100 WHERE id = 'a-old'", [])
                .unwrap();
            conn.execute("UPDATE messages SET date = 300 WHERE id = 'a-new'", [])
                .unwrap();
        }

        let got = core
            .messages_needing_warmup_unified(FolderKind::Inbox, 10)
            .unwrap();
        assert_eq!(
            got,
            vec![
                (AccountId("acc1".into()), MessageId("a-new".into())),
                (AccountId("acc2".into()), MessageId("b1".into())),
                (AccountId("acc1".into()), MessageId("a-old".into())),
            ],
            "newest first across the merge, each id carrying the account the \
             worker has to be asked for"
        );

        // The window spans the merge, exactly like the rows on screen do.
        let capped = core
            .messages_needing_warmup_unified(FolderKind::Inbox, 2)
            .unwrap();
        assert_eq!(
            capped,
            vec![
                (AccountId("acc1".into()), MessageId("a-new".into())),
                (AccountId("acc2".into()), MessageId("b1".into())),
            ],
            "a window of two is two rows of the merged list, not two per account"
        );

        // A kind the merged view does not show is not a view at all.
        assert!(core
            .messages_needing_warmup_unified(FolderKind::Drafts, 10)
            .is_err());
    }

    /// T-097(9): the warm-up list. Three rules matter and each one is a
    /// bug if it breaks -- a message with a body would be re-fetched for
    /// nothing, a message with no `provider_uid` cannot be fetched at all
    /// (the queue would stall on it), and the order must be newest-first
    /// because that is the end of the list the user is looking at.
    #[test]
    fn warmup_asks_inside_a_window_and_never_walks_backwards() {
        let core = Core::memory().unwrap();
        seed_synced_message(&core, "old", "INBOX", 1);
        seed_synced_message(&core, "new", "INBOX", 2);
        seed_synced_message(&core, "cached", "INBOX", 3);
        seed_message(&core, "unsynced", 900);
        {
            let conn = core.db.conn();
            conn.execute("UPDATE messages SET date = 100 WHERE id = 'old'", [])
                .unwrap();
            conn.execute("UPDATE messages SET date = 200 WHERE id = 'new'", [])
                .unwrap();
            conn.execute(
                "UPDATE messages SET date = 300, body_path = 'bodies/c' WHERE id = 'cached'",
                [],
            )
            .unwrap();
        }
        let inbox = FolderId("acc1:inbox".into());

        let ids = core.messages_needing_warmup(&inbox, 10).unwrap();
        assert_eq!(
            ids,
            vec![MessageId("new".into()), MessageId("old".into())],
            "newest first, no cached body, nothing without a provider uid"
        );

        // The window is the newest two rows -- `unsynced` (no provider uid)
        // and `cached` (already has a body). Neither needs warming, and the
        // answer is empty rather than reaching past the window for two rows
        // that do. This is the difference that matters: the old query took
        // "the newest two *without* a body" and so kept walking backwards
        // into the archive every time it was asked again.
        assert!(
            core.messages_needing_warmup(&inbox, 2).unwrap().is_empty(),
            "a warm window must answer empty, not reach past itself"
        );

        // And once the whole window is warm, asking again is a no-op --
        // which is what makes it safe to re-run whenever the folder changes.
        {
            let conn = core.db.conn();
            conn.execute(
                "UPDATE messages SET body_path = 'bodies/x' WHERE id IN ('new', 'old')",
                [],
            )
            .unwrap();
        }
        assert!(
            core.messages_needing_warmup(&inbox, 10).unwrap().is_empty(),
            "re-running a finished warm-up must not queue older mail"
        );

        assert!(core
            .messages_needing_warmup(&FolderId("acc1:archive".into()), 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn open_body_fetches_over_the_session_and_caches_on_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_synced_message(&core, "m1", "INBOX", 42);
        let id = MessageId("m1".into());

        let mut bodies = std::collections::HashMap::new();
        bodies.insert(("INBOX".to_string(), 42), b"hello from the server".to_vec());
        let mut session = FakeSession {
            bodies,
            call_log: Vec::new(),
        };

        let bytes = core.open_body(&mut session, &id, dir.path()).unwrap();
        assert_eq!(bytes, b"hello from the server");
        assert_eq!(
            session.call_log,
            vec!["select:INBOX", "fetch_body:INBOX:42"]
        );

        // Second call is a cache hit: no further session calls at all.
        let bytes2 = core.open_body(&mut session, &id, dir.path()).unwrap();
        assert_eq!(bytes2, b"hello from the server");
        assert_eq!(
            session.call_log,
            vec!["select:INBOX", "fetch_body:INBOX:42"],
            "a cached body must short-circuit before touching the session at all"
        );
    }

    #[test]
    fn open_body_returns_the_cached_copy_without_ever_touching_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_synced_message(&core, "m1", "INBOX", 42);
        let id = MessageId("m1".into());
        core.store_body(&id, dir.path(), b"already cached").unwrap();

        let mut session = FakeSession {
            bodies: std::collections::HashMap::new(),
            call_log: Vec::new(),
        };

        let bytes = core.open_body(&mut session, &id, dir.path()).unwrap();
        assert_eq!(bytes, b"already cached");
        assert!(
            session.call_log.is_empty(),
            "cache hit must never call the session"
        );
    }

    #[test]
    fn open_body_unknown_message_id_is_message_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        let id = MessageId("nope".into());
        let mut session = FakeSession {
            bodies: std::collections::HashMap::new(),
            call_log: Vec::new(),
        };
        let err = core.open_body(&mut session, &id, dir.path()).unwrap_err();
        assert_eq!(err.code, ErrorCode::MessageNotFound);
    }

    #[test]
    fn open_body_with_no_provider_uid_is_invalid_argument_not_a_fetch_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let core = core_with_message("m1", 0);
        let mut core = core;
        let id = MessageId("m1".into());
        let mut session = FakeSession {
            bodies: std::collections::HashMap::new(),
            call_log: Vec::new(),
        };
        let err = core.open_body(&mut session, &id, dir.path()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(
            session.call_log.is_empty(),
            "must not attempt a fetch with no known UID"
        );
    }

    /// A folder that has never been matched against a server LIST has
    /// `remote_id IS NULL`. Fetching then has no honest mailbox name to
    /// ask for, so it must fail like the missing-UID case rather than
    /// silently issuing `SELECT ""` -- proven by the session never being
    /// called at all.
    #[test]
    fn open_body_with_no_folder_remote_id_is_invalid_argument_not_a_fetch_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_synced_message(&core, "m1", "INBOX", 7);
        core.db
            .conn()
            .execute("UPDATE folders SET remote_id = NULL", [])
            .unwrap();

        let mut session = FakeSession {
            bodies: std::collections::HashMap::new(),
            call_log: Vec::new(),
        };
        let err = core
            .open_body(&mut session, &MessageId("m1".into()), dir.path())
            .unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(
            session.call_log.is_empty(),
            "must not touch the server without a real mailbox name, got {:?}",
            session.call_log
        );
    }

    #[test]
    fn open_body_maps_a_session_error_to_network_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        seed_synced_message(&core, "m1", "INBOX", 99);
        let id = MessageId("m1".into());
        let mut session = FakeSession {
            bodies: std::collections::HashMap::new(),
            call_log: Vec::new(),
        };
        let err = core.open_body(&mut session, &id, dir.path()).unwrap_err();
        assert_eq!(err.code, ErrorCode::NetworkUnavailable);
    }
}
