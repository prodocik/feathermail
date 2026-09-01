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
-- `delimiter` (schema v21, T-060t): the server's hierarchy separator for
-- this mailbox, exactly as its LIST response reported it, or NULL when the
-- server reported none (a flat namespace) or the folder has not been
-- discovered yet. It is stored, not interpreted: the one thing Core does
-- with it is split `remote_id` into "path prefix" + "leaf" so a rename can
-- keep a nested folder where it is instead of silently promoting it to the
-- top level. Deriving it from `remote_id` is impossible -- a mailbox name
-- cannot contain its own delimiter, but nothing says which character is it.
CREATE TABLE IF NOT EXISTS folders (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    remote_id TEXT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    parent_id TEXT REFERENCES folders(id),
    color TEXT,
    delimiter TEXT,
    -- T-060u: a folder the user deleted. The row survives because durable
    -- Undo history (`operation_moves`) and `messages` reference it by id, so
    -- it can never actually go away; hiding it is the only truthful local
    -- representation. `remote_id` is cleared only when the server acks the
    -- DELETE, so until then the mailbox still belongs to this row.
    deleted_at INTEGER,
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
    cc TEXT NOT NULL DEFAULT '',
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
    body_bytes INTEGER,
    -- T-102 (schema v24): unix seconds when this body was last *read* out of
    -- the cache (Core::lookup_body on a hit, and the open that fetched it).
    -- messages.date says when the mail was written, which is the wrong key
    -- for "keep what the owner opened": an old message opened a minute ago
    -- was the first thing the size sweep evicted. NULL means never opened.
    body_read_at INTEGER
);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes INTEGER NOT NULL DEFAULT 0,
    cache_path TEXT,
    -- T-111: how many bytes the cached file actually occupies, written
    -- once by `Core::mark_attachment_cached` from the file it just
    -- accepted. `size_bytes` is the server's octet count for the encoded
    -- part and is not the same number; the attachment cache budget is
    -- summed from this column, so nothing else may write it.
    cache_bytes INTEGER,
    content_id TEXT,
    -- T-043: exact IMAP BODY.PEEK section and transfer encoding. The
    -- attachment cache fetches only this part and never needs the full
    -- RFC822 message in memory.
    part_path TEXT,
    transfer_encoding TEXT NOT NULL DEFAULT 'identity'
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
    -- v28: `ON DELETE SET NULL`, not the default NO ACTION and not CASCADE.
    -- A draft is local data the owner typed and D29 says we never lose it,
    -- so it cannot ride the thread out; but the thread it answers does get
    -- deleted underneath it by ordinary sync (a UIDVALIDITY reset, the last
    -- message of the thread vanishing on the server, a de-duplicated copy),
    -- and NO ACTION turned every one of those into `FOREIGN KEY constraint
    -- failed` -- rolling back the whole sync transaction and wedging the
    -- folder forever. The link simply stopped being true; the draft stays,
    -- with `in_reply_to` still carrying what it is a reply to.
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
    -- T-042: durable revision for a queued APPEND. It is intentionally
    -- independent from `updated_at`: autosaves can share one wall-clock
    -- second, but each must still supersede the prior queued upload.
    sync_revision INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS draft_attachments (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    draft_id TEXT NOT NULL REFERENCES drafts(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes INTEGER NOT NULL,
    source_path TEXT NOT NULL
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
    -- T-157 (schema v29): the rolling full-mailbox UID reconciliation
    -- cursor. Every completed pass re-checks the newest
    -- `feathermail_sync::RECONCILE_WINDOW` UIDs, which is where mail is
    -- actually read and deleted -- but on a CONDSTORE server nothing else
    -- ever looked below that window, so a message deleted deep in the
    -- mailbox stayed local forever (a CONDSTORE delta reports changed
    -- flags, not gone mail). `resync_cursor` is the highest UID the walk
    -- has not checked yet; it starts just below the newest window and
    -- moves down one `UID_FETCH_BATCH` per pass, so the whole mailbox is
    -- covered eventually at a cost of exactly one extra `UID FETCH` per
    -- pass and never a `1:*` sweep. NULL means no walk is in progress:
    -- either none has ever started, or the last one reached UID 1 and
    -- stamped `resync_completed_at`. The next circle then waits
    -- `feathermail_sync::FULL_RECONCILE_INTERVAL_SECS` from that stamp --
    -- a separate clock from `last_sync_at` above, which moves on every
    -- successful pass and so can never say when a circle closed.
    resync_cursor INTEGER,
    resync_completed_at INTEGER,
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
    -- T-162 (schema v29): the queue's own monotonic sequence number, handed
    -- out by `feathermail_core::store::enqueue` on every INSERT *and* on
    -- every revive. `created_at` is whole seconds, so two commands issued
    -- back to back usually tie and the tie-break decides the real order;
    -- `claim_next` used `rowid` for that, which SQLite hands out in INSERT
    -- order and never changes afterwards. A revived row (D29's idempotency
    -- key lands a repeated command on its own `failed`/`acked` row, which
    -- `enqueue` turns back into `pending` with an UPDATE) therefore kept
    -- the rowid of the first time it was issued and was claimed ahead of
    -- everything enqueued since -- a Star from ten minutes ago applied
    -- after a Move issued a second ago. `seq` is `MAX(seq) + 1` at the
    -- moment the operation last became claimable, which is exactly the
    -- order the user issued the commands in.
    seq INTEGER,
    -- T-034: causal predecessor for a reverse operation created by Undo.
    -- Status is intentionally data-driven (pending/running/acked/failed/
    -- blocked/cancelled/local); old profiles are upgraded additively in
    -- lib.rs. `local` is a Core-only ledger state and is never claimed by
    -- a provider worker.
    undo_of TEXT REFERENCES operations(id),
    -- T-034: monotonic audit point for the user's Undo request. It is set
    -- before the cancellation/reverse transition commits.
    undo_requested_at INTEGER,
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

-- T-076 (schema v10): durable per-message MOVE intent. A local optimistic
-- folder change must not discard the source `(remote_id, UID)` before the
-- provider has applied it. `destination_uid` is NULL until destination sync
-- observes the server's copy; the same `messages.id` is then rehomed
-- instead of a second logical message/thread being inserted.
CREATE TABLE IF NOT EXISTS operation_moves (
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

-- T-034: append-only copy of move coordinates. Active intents in
-- operation_moves are deleted after source/destination reconciliation; Undo
-- still needs the original server coordinates after that point.
CREATE TABLE IF NOT EXISTS operation_move_history (
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
    UNIQUE (operation_id, message_id, source_folder_id, source_uid,
            destination_folder_id, destination_uid)
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
    -- T-059: coarse default policy.  Per-action grants remain in
    -- mcp_permissions; a launching process can only reduce this policy.
    permission_level TEXT NOT NULL DEFAULT 'draft',
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS mcp_permissions (
    client_id TEXT NOT NULL REFERENCES mcp_clients(id) ON DELETE CASCADE,
    capability TEXT NOT NULL,
    grant TEXT NOT NULL,
    PRIMARY KEY (client_id, capability)
);

-- T-059: bounded cross-process approval hand-off for MCP stdio and the
-- GTK window.  Opaque identifiers/revisions make an approval specific to
-- one action without storing request arguments, mail bodies, paths or secrets.
CREATE TABLE IF NOT EXISTS mcp_confirmation_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    client_id TEXT NOT NULL REFERENCES mcp_clients(id) ON DELETE CASCADE,
    capability TEXT NOT NULL,
    account_id TEXT REFERENCES accounts(id) ON DELETE CASCADE,
    target_id TEXT,
    target_count INTEGER NOT NULL DEFAULT 1,
    fingerprint TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    resolved_at INTEGER
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
    -- v28: `ON DELETE SET NULL`, same reasoning as `drafts.thread_id` and
    -- for the same class of bug. T-041/T-045 make an outbox row a frozen,
    -- self-contained snapshot: once it exists it never reads the draft
    -- again, so the pointer back is a provenance breadcrumb, not a
    -- dependency. Under NO ACTION that breadcrumb made `Core::delete_draft`
    -- impossible for any draft that had ever been sent -- the discard came
    -- back as "Couldn't save that change." and the row stayed forever.
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

CREATE TABLE IF NOT EXISTS outbox_attachments (
    outbox_id TEXT NOT NULL REFERENCES outbox(id) ON DELETE CASCADE,
    filename TEXT NOT NULL,
    mime TEXT NOT NULL DEFAULT 'application/octet-stream',
    size_bytes INTEGER NOT NULL,
    source_path TEXT NOT NULL,
    PRIMARY KEY (outbox_id, filename, source_path)
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
-- Search spans folders, unlike the Inbox page above. Its FTS path pages
-- newest threads after a membership lookup, so it needs this ordering index
-- rather than borrowing the folder-prefixed one and sorting every match.
CREATE INDEX IF NOT EXISTS threads_account_date
    ON threads (account_id, date DESC, id DESC);
-- T-136: the merged view has no `account_id` to lead with, so every index
-- above is unusable for it and the sidebar's four unified counts each
-- became a full scan of `threads` -- 261 818 rows on the owner's profile,
-- about 1.8 s per recount on a mailbox that is also being written to. The
-- same scan sits inside the merged warm-up's join. Leading with
-- `folder_id` turns `folder_id IN (…) AND deleted = 0 AND archived = 0 AND
-- snooze_until IS NULL` into a seek, and carrying `unread` at the end
-- makes it *covering*: `COUNT(*)`/`SUM(unread)` never touch the table.
-- Column order is the query's own: equality columns first, the NULL test
-- next, the summed column last. Costs about 11 MB on a 520 MB profile,
-- and is built once, by this batch, on the first open after the upgrade.
CREATE INDEX IF NOT EXISTS threads_folder_flags
    ON threads (folder_id, deleted, archived, snooze_until, unread);
CREATE INDEX IF NOT EXISTS threads_account_unread
    ON threads (account_id, unread) WHERE unread = 1;
CREATE INDEX IF NOT EXISTS threads_account_starred
    ON threads (account_id, starred) WHERE starred = 1;
CREATE INDEX IF NOT EXISTS threads_snooze_until
    ON threads (snooze_until) WHERE snooze_until IS NOT NULL;

CREATE INDEX IF NOT EXISTS messages_account_thread
    ON messages (account_id, thread_id);
-- Thread rollup asks `WHERE m.thread_id = ?` without an account: seven
-- correlated subqueries per thread in `CoreSyncStore::rollup_folder`, plus
-- the `UPDATE messages SET thread_id` that retargeting runs per absorbed
-- thread. `messages_account_thread` cannot serve any of them -- `thread_id`
-- is not its leading column -- so every one of them scanned the whole table,
-- and one 200-header sync batch is ~1400 of those scans. That cost grows
-- with the size of the mailbox, not with the size of the batch, which is why
-- it stayed invisible on small profiles and pinned a core on a real one:
-- measured on a 205 909-message profile, one batch went from 71.0 s to
-- 58 ms with this index present. `date DESC, id DESC` follows `thread_id`
-- because three of those subqueries are `ORDER BY m.date DESC, m.id DESC
-- LIMIT 1` -- with the ordering in the index they are a single seek instead
-- of a sort of the whole thread.
CREATE INDEX IF NOT EXISTS messages_thread_date
    ON messages (thread_id, date DESC, id DESC);
CREATE INDEX IF NOT EXISTS draft_attachments_draft
    ON draft_attachments (draft_id, id);
CREATE INDEX IF NOT EXISTS messages_account_provider_uid
    ON messages (account_id, provider_uid);
CREATE UNIQUE INDEX IF NOT EXISTS messages_account_folder_provider_uid
    ON messages (account_id, folder_id, provider_uid)
    WHERE provider_uid IS NOT NULL;
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
CREATE INDEX IF NOT EXISTS operation_moves_source_locator
    ON operation_moves (source_folder_id, source_uid);
CREATE INDEX IF NOT EXISTS operation_moves_destination_locator
    ON operation_moves (destination_folder_id, destination_uid);
CREATE INDEX IF NOT EXISTS operation_moves_message
    ON operation_moves (message_id);
CREATE INDEX IF NOT EXISTS operation_move_history_operation
    ON operation_move_history (operation_id, history_id);
CREATE INDEX IF NOT EXISTS operation_move_history_message
    ON operation_move_history (message_id, history_id);
CREATE INDEX IF NOT EXISTS search_history_created ON search_history (created_at DESC);
CREATE INDEX IF NOT EXISTS mcp_audit_created ON mcp_audit (created_at DESC);
CREATE INDEX IF NOT EXISTS mcp_confirmation_pending
    ON mcp_confirmation_requests (status, created_at, id);
CREATE INDEX IF NOT EXISTS mcp_confirmation_fingerprint
    ON mcp_confirmation_requests (client_id, capability, fingerprint, status);
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

-- T-068: FTS5 keeps `message_id` UNINDEXED so it cannot be used as a
-- searchable mail term. A plain `DELETE ... WHERE message_id = ?` therefore
-- scans the entire virtual table for every reindex. This normal SQLite map
-- gives lifecycle code the FTS rowid to delete directly. It is not mail
-- content: the opaque message id already exists in `messages`; the FK keeps
-- the map from outliving its message.
CREATE TABLE IF NOT EXISTS fts_message_rows (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    fts_rowid INTEGER NOT NULL UNIQUE
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

-- T-093 (schema v9): no DDL change here -- `fts_pending`'s shape above is
-- unchanged since v8. The version bump is data-only: `Database::migrate`'s
-- `current < 9` block re-queues every message with a cached body into
-- `fts_pending`, because `index_one` used to write raw undecoded RFC822
-- bytes into `messages_fts.body` instead of parsed text, so rows indexed
-- before this fix need re-indexing, not just new mail. A fresh install has
-- no rows in `messages` yet, so that block is a no-op here and this schema
-- stays byte-for-byte what a v8 install already had.

-- T-096 (schema v23): the same again, one content type further along, and
-- again data-only. `index_one` used to write an empty `body` for a message
-- whose only displayable part is `text/html`, so every such message already
-- in `messages_fts` is findable by sender and subject but not by a word it
-- contains. `Database::migrate`'s `current < 23` block re-queues every
-- message with a cached body for the same reason the v9 block did, and with
-- the same no-op effect on a fresh install.

-- T-155 (schema v29): the same again, once more data-only. The text
-- normalisation `messages_fts` is fed with changed, so every row indexed by
-- the old one answers a different set of queries than a row indexed today.
-- `Database::migrate`'s `current < 29` block re-queues *every* message, not
-- just those with a cached body as in v9/v23: normalisation applies to the
-- subject and the addresses too, which are indexed for a message whose body
-- was never downloaded. On a fresh install `messages` is empty and the
-- block is a no-op, so this file stays what a v28 install already had.

-- T-060s (schema v20): the one durable door a *headless* process has for
-- "sync this account now". The stdio MCP server runs in its own process
-- (`crates/mcp/src/main.rs` opens its own `Core`); it has no handle on the
-- GTK shell's `feathermail_service::SyncHandle`, so it cannot send on that
-- channel. It writes a row here instead, and the shell -- which is already
-- polling Core twice a second for pending MCP confirmations -- claims and
-- deletes it, then wakes the worker exactly the way the Diagnostics "Sync
-- now" button does.
--
-- The primary key is the account, not a request id: two agents asking for
-- a sync a second apart want one sync, not two, and a claim that finds one
-- row has lost nothing. `requested_at` is kept for the same reason the
-- confirmation table keeps `created_at` -- so a stale request from a shell
-- that was never running can be recognised, not so it can be sorted.
-- Nothing about mail lives here: an opaque account id and a timestamp.
CREATE TABLE IF NOT EXISTS sync_requests (
    account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    requested_at INTEGER NOT NULL
);

-- T-134 (schema v26): the queue of snippets that have to be recomputed.
--
-- `preview::html_to_text` used to collect entities *inside* tags, so an
-- HTML letter whose image URLs carry `&`-separated tracking parameters had
-- every one of those ampersands emptied into the preview -- the owner saw
-- rows reading "&&&&&&&& then the text of the letter". The parser is fixed,
-- but a snippet is stored, not derived on read: the rows already written
-- stay wrong until something recomputes them from the cached MIME body.
--
-- Same shape and same reason as `fts_pending` next to it: a queue, drained
-- in bounded batches by the background worker (`Core::repair_snippet_batch`),
-- never on the GTK thread -- reading a body file per row is disk work. Rows
-- are deleted as they are handled whether or not the recomputed snippet
-- differs, so the queue always empties; `Database::migrate`'s `current < 26`
-- block seeds it from the rows that actually look damaged, and on a fresh
-- install `messages` is empty and the block is a no-op.
CREATE TABLE IF NOT EXISTS snippet_repairs (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE
);
