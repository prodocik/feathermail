//! T-048: FTS5 indexing in the background, and executing a
//! [`feathermail_search::SearchPlan`] against `messages_fts`.
//!
//! `feathermail-search` (T-047) parses the D54 query language and hands
//! back data (`fts_match`, `predicates`) — it deliberately has no
//! `rusqlite` dependency and never touches SQLite itself (see its module
//! doc). This module is the other half: the one place in the workspace
//! that turns that data into SQL and runs it. That is a new D9 edge
//! (`core` -> `search`), so `scripts/check-layering.sh` gained a
//! `require_no_feathermail_deps search` rule in the same change — verified
//! by reading `crates/search/Cargo.toml` first, not assumed.
//!
//! ## Two separate problems, one queue
//!
//! "Index in the background" is really two different facts about a
//! message that both have to reach `messages_fts` without blocking
//! whatever is happening when they become true:
//!
//! 1. **A message appeared.** IMAP sync (`crates/core/src/sync_store.rs`,
//!    `CoreSyncStore::upsert_one`'s new-message branch) writes a `messages`
//!    row the instant headers arrive — the body may not even be
//!    downloaded yet (T-024 fetches it lazily, on open). Indexing must not
//!    make that write wait on anything, especially not disk I/O for a body
//!    that might not exist yet.
//! 2. **A body arrived later.** [`crate::body::Core::store_body`] (T-024)
//!    caches a body well after the message row was created, often when the
//!    user opens the thread. Whatever `messages_fts` already has for that
//!    message (indexed with an empty body, from case 1) is now stale and
//!    needs the body's text folded in.
//!
//! Both cases reduce to the same fact: "`messages_fts` does not reflect
//! this `message_id`'s current state, go make it." So both write to the
//! same `fts_pending` queue (schema v8, `message_id` primary key —
//! re-enqueuing an already-pending row is a harmless no-op via `INSERT OR
//! IGNORE`, the eventual index pass just picks up whatever is in
//! `messages` by then). [`Core::index_pending_batch`] drains it in
//! `queued_at` order, a bounded number of rows at a time, so a caller
//! (`crates/app`/`crates/service`, out of bounds for this task — see the
//! report) can drive it from an idle callback or a timer without ever
//! blocking the thread that keeps Inbox responsive. [`Core::search`]
//! reports `pending_index` (the queue's current length) alongside results,
//! so a caller can distinguish "no results" from "no results *yet*, N
//! messages still indexing" instead of quietly lying about completeness.
//!
//! ## Why not FTS5 external-content + triggers
//!
//! SQLite's usual answer to "keep an FTS index in sync automatically" is
//! an `external content` table plus `AFTER INSERT/UPDATE/DELETE` triggers
//! on the content table, so every write to `messages` updates
//! `messages_fts` for free with no application code involved. Rejected
//! here for two independent reasons, either one sufficient on its own:
//!
//! - **Rowid alignment.** External-content fts5 requires the content
//!   table's rowid to line up with the FTS table's rowid (or an explicit
//!   `content_rowid=`). `messages.id` is a `TEXT` id (`"msg:<account>:
//!   <folder>:<uid>"`, see `CoreSyncStore::message_row_id`), not an
//!   `INTEGER PRIMARY KEY` — there is no rowid to align with. Making one
//!   work would mean adding a shadow integer id to `messages` (a real
//!   schema change with its own migration and its own way to drift out of
//!   sync) just to satisfy fts5's rowid requirement, before triggers even
//!   enter the picture.
//! - **Triggers fire at the wrong time regardless.** Even past the rowid
//!   problem, an `AFTER INSERT` trigger on `messages` fires at exactly the
//!   moment case 1 above happens — when the body is `NULL`. It would index
//!   an empty body once and never automatically again; case 2 (the body
//!   arriving later, via `UPDATE messages SET body_path = ...`) would
//!   still need an `AFTER UPDATE OF body_path` trigger that itself has to
//!   read the cached body *off disk* — inside a trigger, synchronously,
//!   on whatever thread called `store_body`, which is precisely the
//!   "must not block" property T-048 exists to protect. Triggers would
//!   only ever handle the metadata half of this problem automatically;
//!   the body half needs hand-written code either way, and once that code
//!   exists, running it through the same explicit queue for *both* cases
//!   is one mechanism instead of two (triggers for metadata, manual code
//!   for bodies) that have to be kept in each other's story straight.
//!
//! ## Account isolation (critical — see the report's mutation on this)
//!
//! `messages_fts` has no `account_id` column (an fts5 virtual table
//! property documented on the table itself and on
//! `feathermail_db::lib`'s `every_table_is_accounted_for_by_remove_account_or_an_explicit_reason`
//! test) — a `MATCH` by itself can return another account's message. Every
//! query in [`Core::search`] therefore joins `messages_fts` to `messages`
//! and filters *both* `threads.account_id` and `messages.account_id` to
//! the caller's account before anything is returned; there is no code path
//! here that runs a bare `messages_fts` query. See
//! `search_never_returns_another_accounts_message` below.
//!
//! That test alone does not pin *which* of the three account_id checks
//! (the JOIN's `m.account_id = t.account_id`, and WHERE's own two) is
//! actually doing the work — as of today none of them individually is:
//! `CoreSyncStore::thread_row_id` bakes the account into every thread id,
//! so a `messages` row and its own thread always agree, and any single
//! one of the three checks is enough given that invariant. Accepted found
//! this during review (2026-08-22) and asked for the honest version, plus
//! a test that actually exercises the state these checks exist for. See
//! the long comment on the `WHERE` clause in [`Core::search`] and
//! `search_never_returns_a_thread_whose_only_matching_message_belongs_to_a_different_account`
//! below, which builds a `messages` row that disagrees with its own
//! thread's `account_id` by hand (today's write path cannot produce one)
//! and only fails once all three checks are gone at once — removing any
//! one alone still leaves it green, because one of the other two is still
//! standing. Kept as three anyway: T-029 is expected to replace
//! `thread_row_id` with real threading, at which point the invariant this
//! relies on stops holding and defense-in-depth stops being redundant.
//!
//! ## D14
//!
//! [`SearchResults`] holds `Vec<Thread>` — `Thread` already derives
//! `Debug` (it flows through `Core::list_threads`/`get_thread` today) and
//! carries subject/preview/body text, so this module does not change what
//! is reachable through *that* type's `Debug`. What is new here is kept
//! off it: [`SearchResults`] itself has a hand-written `Debug` that prints
//! only counts, never the matched threads, and no error path in this
//! module ever puts a query's matched text (subject, body, sender, the
//! raw `fts_match` string) into a `CoreError` — only `rusqlite::Error`'s
//! own text (SQL shape, not row content) via [`sql_err`].

use std::path::Path;

use feathermail_search::{Addressee, Date, IsFlag, Predicate, SearchPlan};
use rusqlite::{params, OptionalExtension};

use crate::error::CoreError;
use crate::model::AccountId;
use crate::model::{Thread, ThreadCursor};
use crate::store::{map_thread, sql_err, Core, THREAD_COLUMNS, THREAD_LATEST_JOIN};

/// [`Core::index_pending_batch`]'s default batch size when a caller has no
/// better number. Small enough that one call is not itself a long block
/// (a body read is at most one file read per row), large enough that
/// draining a mailbox with thousands of backlogged messages does not need
/// thousands of round trips.
pub const DEFAULT_INDEX_BATCH: usize = 200;

/// What one [`Core::index_pending_batch`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexBatchResult {
    /// Rows removed from `fts_pending` and written to `messages_fts` by
    /// this call.
    pub indexed: usize,
    /// Rows still left in `fts_pending` immediately after this call. Zero
    /// means the index is fully caught up as of this moment (a message
    /// enqueued a heartbeat later is not a bug, just the next batch's
    /// job).
    pub remaining: usize,
}

/// The result of [`Core::search`]: matching threads for one account, plus
/// whether the index that produced them is known-complete right now.
///
/// Deliberately not `Debug`-derived — see the module doc's D14 section.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct SearchResults {
    /// Matching threads, newest first, already scoped to the account the
    /// caller asked for (see the module doc's "Account isolation"
    /// section) — never another account's mail.
    pub threads: Vec<Thread>,
    /// `fts_pending`'s row count at the moment this search ran, workspace
    /// wide (the queue has no `account_id` either, same shape as
    /// `messages_fts`; a single indexer drains every account's backlog).
    /// Non-zero means: some mail exists that this search could not have
    /// matched against yet, because it has not been indexed. Zero does
    /// *not* guarantee every match was found (a message could be enqueued
    /// again a moment after this count was read), only that nothing was
    /// *known* to be missing as of this call.
    pub pending_index: usize,
    /// T-049 (pagination): `Some(cursor)` when `threads` was cut short by
    /// `limit` and there is at least one more matching thread beyond it —
    /// pass it back as [`Core::search`]'s `after` argument to fetch the
    /// next page. `None` means this page reached the end of what matches
    /// today, not that nothing more will ever match (the same caveat
    /// `pending_index` already carries: a background index catching up,
    /// or new mail arriving, can make a later call find more).
    ///
    /// Reuses [`ThreadCursor`] — the exact same (date, id) cursor
    /// [`Core::list_threads`] already hands back for `ThreadPage::next`
    /// — rather than inventing a second pagination token type for the
    /// same underlying `ORDER BY t.date DESC, t.id DESC` ordering this
    /// query also uses.
    pub next: Option<ThreadCursor>,
}

impl std::fmt::Debug for SearchResults {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchResults")
            .field("thread_count", &self.threads.len())
            .field("pending_index", &self.pending_index)
            .field("has_next_page", &self.next.is_some())
            .finish()
    }
}

impl Core {
    /// Execute `plan` against `account_id`'s mail. `limit` caps the number
    /// of threads returned per page (0 means "use a sane default");
    /// results are ordered newest-first, same as [`Core::list_threads`].
    /// `after` is `None` for the first page, or a cursor from a previous
    /// call's [`SearchResults::next`] to continue where that page left
    /// off — same pagination shape as [`Core::list_threads`]/
    /// [`crate::model::ListThreadsQuery::after`], reusing
    /// [`ThreadCursor`] rather than a second cursor type for the same
    /// (date, id) ordering.
    ///
    /// `Err(AccountNotFound)` for an unknown account, same as every other
    /// account-scoped `Core` method (see [`Core::require_account`]).
    pub fn search(
        &self,
        account_id: &AccountId,
        plan: &SearchPlan,
        after: Option<&ThreadCursor>,
        limit: usize,
    ) -> Result<SearchResults, CoreError> {
        self.require_account(account_id.as_str())?;
        let limit = if limit == 0 { 50 } else { limit };
        let (sql, binds) = self.build_search_sql(account_id, plan, after, limit)?;
        let conn = self.db.conn();

        let mut stmt = conn.prepare(&sql).map_err(sql_err)?;
        let p: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let mut threads = stmt
            .query_map(p.as_slice(), map_thread)
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        drop(stmt);

        // Fetched `limit + 1` rows (see `build_search_sql`): a `limit`th
        // + 1 row means there is at least one more match beyond this
        // page, same "over-fetch by one, trim, cursor the last kept row"
        // technique `Core::list_threads` already uses for `ThreadPage`.
        let next = if threads.len() > limit {
            threads.truncate(limit);
            threads.last().map(ThreadCursor::of)
        } else {
            None
        };

        let pending_index: i64 = conn
            .query_row("SELECT COUNT(*) FROM fts_pending", [], |row| row.get(0))
            .map_err(sql_err)?;

        Ok(SearchResults {
            threads,
            pending_index: pending_index.max(0) as usize,
            next,
        })
    }

    /// Build the SQL text and bound parameters [`Core::search`] runs.
    /// Split out so the EXPLAIN QUERY PLAN test below
    /// (`search_with_a_free_text_term_reaches_the_fts_index_not_a_full_table_scan`)
    /// runs the *exact* query a real search issues rather than a
    /// hand-copied approximation that could silently drift from it.
    fn build_search_sql(
        &self,
        account_id: &AccountId,
        plan: &SearchPlan,
        after: Option<&ThreadCursor>,
        limit: usize,
    ) -> Result<(String, Vec<Box<dyn rusqlite::ToSql>>), CoreError> {
        if let Some(fts_match) = &plan.fts_match {
            // FTS can match tens of thousands of messages for an ordinary
            // broad word. Materialize matching thread ids as an `IN` set,
            // page the lightweight `threads` rows in the account/date index,
            // *then* project each page row's latest message. Putting
            // THREAD_LATEST_JOIN in the first SELECT used to run its
            // correlated date sort for every FTS match before LIMIT, so a
            // broad 50k search missed §61 even though the UI only needs one
            // page. The CTE preserves exactly the same newest-first thread
            // semantics and account boundary; it merely makes both ordering
            // and the expensive latest projection page-bounded.
            let mut matching_sql = "m.account_id = ? AND messages_fts MATCH ?".to_string();
            let mut matching_binds: Vec<Box<dyn rusqlite::ToSql>> = vec![
                Box::new(account_id.as_str().to_string()),
                Box::new(fts_match.clone()),
            ];
            for predicate in &plan.predicates {
                match predicate {
                    Predicate::Is(IsFlag::Unread) => matching_sql.push_str(" AND m.unread = 1"),
                    Predicate::Is(IsFlag::Read) => matching_sql.push_str(" AND m.unread = 0"),
                    Predicate::Is(IsFlag::Starred) => matching_sql.push_str(" AND m.starred = 1"),
                    Predicate::HasAttachment => matching_sql.push_str(" AND m.has_attachment = 1"),
                    Predicate::To(addressee) => {
                        let needle = match addressee {
                            Addressee::Me => self.account_email(account_id)?,
                            Addressee::Address(addr) => addr.clone(),
                        };
                        matching_sql.push_str(" AND m.recipients LIKE ?");
                        matching_binds.push(Box::new(format!("%{needle}%")));
                    }
                    Predicate::After(date) => {
                        matching_sql.push_str(" AND m.date >= ?");
                        matching_binds.push(Box::new(epoch_seconds_for_date(date)));
                    }
                    Predicate::Before(date) => {
                        matching_sql.push_str(" AND m.date < ?");
                        matching_binds.push(Box::new(epoch_seconds_for_date(date)));
                    }
                }
            }

            let mut page_sql = format!(
                "t.account_id = ? AND t.id IN ( \
                 SELECT m.thread_id FROM messages m \
                 JOIN messages_fts f ON f.message_id = m.id \
                 WHERE {matching_sql} \
                 )"
            );
            let mut binds: Vec<Box<dyn rusqlite::ToSql>> =
                vec![Box::new(account_id.as_str().to_string())];
            binds.append(&mut matching_binds);
            if let Some(cursor) = after {
                page_sql.push_str(" AND (t.date < ? OR (t.date = ? AND t.id < ?))");
                binds.push(Box::new(cursor.date));
                binds.push(Box::new(cursor.date));
                binds.push(Box::new(cursor.id.as_str().to_string()));
            }
            binds.push(Box::new(limit as i64 + 1));
            let sql = format!(
                "WITH page AS ( \
                     SELECT t.id, t.date \
                     FROM threads t \
                     WHERE {page_sql} \
                     ORDER BY t.date DESC, t.id DESC LIMIT ? \
                 ) \
                 SELECT {THREAD_COLUMNS} \
                 FROM page \
                 JOIN threads t ON t.id = page.id \
                 {THREAD_LATEST_JOIN} \
                 ORDER BY page.date DESC, page.id DESC"
            );
            return Ok((sql, binds));
        }

        let mut sql = format!(
            "SELECT DISTINCT {THREAD_COLUMNS} \
             FROM threads t {THREAD_LATEST_JOIN} \
             JOIN messages m ON m.thread_id = t.id AND m.account_id = t.account_id"
        );
        if plan.fts_match.is_some() {
            sql.push_str(" JOIN messages_fts f ON f.message_id = m.id");
        }
        // Three account_id checks stack here: the JOIN's own
        // `m.account_id = t.account_id`, and this WHERE's `t.account_id`
        // and `m.account_id`. As of today, at most one of them is ever
        // load-bearing at a time -- `CoreSyncStore::thread_row_id` bakes
        // the account into every thread id ("thr:{account}:{folder}:
        // {uid}"), so a `messages` row and its own `threads` row always
        // agree on `account_id` in every row this crate's write path can
        // produce, which makes any one of the three sufficient on its own
        // and the other two provably redundant *given that invariant*.
        // Kept anyway, on purpose: T-029 (real `References`/`In-Reply-To`
        // threading) will very likely replace `thread_row_id` with
        // something that no longer bakes the account in, at which point a
        // `messages` row disagreeing with its own thread's `account_id`
        // stops being hypothetical -- and whichever of these three checks
        // is still standing at that point is the one that keeps a
        // mismatched row from surfacing. Bound as an ordinary parameter
        // (never string-interpolated) same as every other value this
        // method adds to `sql`. Pinned by
        // `search_never_returns_a_thread_whose_only_matching_message_belongs_to_a_different_account`,
        // which builds exactly that disagreeing row by hand (the write
        // path cannot produce it) and fails only once *all three* checks
        // are gone at once -- removing the JOIN's tie alone, or WHERE's
        // `m.account_id` alone, each leaves that test green, because the
        // other survivor still blocks it. Do not read that as permission
        // to drop the "spare" ones: it demonstrates today's redundancy,
        // not that any one of them is safe to remove pre-emptively.
        sql.push_str(" WHERE t.account_id = ? AND m.account_id = ?");

        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(account_id.as_str().to_string()),
            Box::new(account_id.as_str().to_string()),
        ];

        if let Some(m) = &plan.fts_match {
            // `messages_fts MATCH ?`, not `f MATCH ?`: fts5's special
            // "table MATCH expr" syntax only recognizes the virtual
            // table's real name, not a query alias -- confirmed against a
            // toy fts5 table before writing this (`f` alone comes back
            // "no such column: f" the moment the join key is
            // `message_id`, an ordinary UNINDEXED column, rather than
            // `rowid`). `f.message_id` in the JOIN above is unaffected;
            // only the MATCH operand needs the real name.
            sql.push_str(" AND messages_fts MATCH ?");
            binds.push(Box::new(m.clone()));
        }

        for predicate in &plan.predicates {
            match predicate {
                Predicate::Is(IsFlag::Unread) => sql.push_str(" AND m.unread = 1"),
                Predicate::Is(IsFlag::Read) => sql.push_str(" AND m.unread = 0"),
                Predicate::Is(IsFlag::Starred) => sql.push_str(" AND m.starred = 1"),
                Predicate::HasAttachment => sql.push_str(" AND m.has_attachment = 1"),
                Predicate::To(addressee) => {
                    // `to:me` resolves here, not in `feathermail-search`
                    // (which has no way to know the signed-in account's
                    // address — see that crate's doc on `Addressee`).
                    // `Core` has it: `accounts.email`.
                    let needle = match addressee {
                        Addressee::Me => self.account_email(account_id)?,
                        Addressee::Address(addr) => addr.clone(),
                    };
                    sql.push_str(" AND m.recipients LIKE ?");
                    binds.push(Box::new(format!("%{needle}%")));
                }
                Predicate::After(date) => {
                    sql.push_str(" AND m.date >= ?");
                    binds.push(Box::new(epoch_seconds_for_date(date)));
                }
                Predicate::Before(date) => {
                    sql.push_str(" AND m.date < ?");
                    binds.push(Box::new(epoch_seconds_for_date(date)));
                }
            }
        }

        // T-049 pagination: same cursor shape and comparison
        // `Core::list_threads` already uses for `ThreadPage` — strictly
        // "older" in (date DESC, id DESC) order than the last thread the
        // caller already has. Placed after every predicate above (order
        // does not matter for correctness, SQLite reorders `AND`
        // clauses freely) but before `ORDER BY`/`LIMIT`, matching
        // `Core::list_threads`'s own clause order.
        if let Some(cursor) = after {
            sql.push_str(" AND (t.date < ? OR (t.date = ? AND t.id < ?))");
            binds.push(Box::new(cursor.date));
            binds.push(Box::new(cursor.date));
            binds.push(Box::new(cursor.id.as_str().to_string()));
        }

        sql.push_str(" ORDER BY t.date DESC, t.id DESC LIMIT ?");
        // Over-fetch by one so `Core::search` can tell "exactly `limit`
        // matches, no more" apart from "there is at least one more page"
        // without a second COUNT query.
        binds.push(Box::new(limit as i64 + 1));

        Ok((sql, binds))
    }

    fn account_email(&self, account_id: &AccountId) -> Result<String, CoreError> {
        self.db
            .conn()
            .query_row(
                "SELECT email FROM accounts WHERE id = ?1",
                params![account_id.as_str()],
                |row| row.get(0),
            )
            .map_err(sql_err)
    }

    /// T-049 (д): append one entry to `search_history` (schema v8's
    /// `search_history (id, query, created_at)`, previously unused by any
    /// caller in this workspace). `query` is trimmed first; a blank query
    /// (nothing typed, or a debounce firing on whitespace) is a no-op, not
    /// an empty row.
    ///
    /// D14: a search query is the content of what the signed-in human
    /// typed to search their own mail — not a secret, but not something
    /// that belongs in a log line or an error message either, same
    /// footing as a thread subject. This method writes it to the local
    /// profile database only (never to `Debug`, never to `CoreError`,
    /// never over MCP — there is no MCP tool for search history, see
    /// `docs/capability-matrix.md`'s "Search history" row).
    ///
    /// What actually reaches this method is the caller's decision, not
    /// this crate's: `crates/app`'s `Msg::SearchDebounced` handler is the
    /// one call site, and it calls this once per *debounced* query the
    /// user actually left the search box on — not once per keystroke, and
    /// not for every debounce firing that turned out to be superseded
    /// before Core ever saw it (`search_gen` drops those first). A method
    /// that recorded on every keystroke would turn "invoice" into eight
    /// history rows for one search.
    pub fn record_search_history(&self, query: &str) -> Result<(), CoreError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(());
        }
        self.db
            .conn()
            .execute(
                "INSERT INTO search_history (query, created_at) VALUES (?1, strftime('%s','now'))",
                params![query],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// The most recent distinct queries from `search_history`, newest
    /// first, for the search popover's history list (T-049). `limit`
    /// caps the count (0 means "use a sane default"). Bounded by `LIMIT`
    /// and backed by `search_history_created` (`created_at DESC`) — not
    /// an unbounded scan of the whole history table.
    ///
    /// Deduplicated (a query searched five times shows once, at its most
    /// recent position) by over-fetching a bounded multiple of `limit`
    /// raw rows and folding duplicates client-side, since `search_history`
    /// is an append-only log by design (every entry keeps its own
    /// `created_at`) rather than an upserted "last seen" table.
    pub fn list_search_history(&self, limit: usize) -> Result<Vec<String>, CoreError> {
        let limit = if limit == 0 { 8 } else { limit };
        let conn = self.db.conn();
        let mut stmt = conn
            .prepare("SELECT query FROM search_history ORDER BY created_at DESC, id DESC LIMIT ?1")
            .map_err(sql_err)?;
        let raw = stmt
            .query_map(params![(limit * 4) as i64], |row| row.get::<_, String>(0))
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(limit);
        for q in raw {
            if seen.insert(q.clone()) {
                out.push(q);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Drain up to `limit` rows off `fts_pending` (see the module doc),
    /// (re)building each one's `messages_fts` row from `messages`'
    /// *current* state: `sender_name`/`sender_email` -> `sender`,
    /// `recipients` -> `recipients`, `subject` -> `subject`,
    /// `attachments.filename` -> `attachment_names`, joined
    /// `labels.name` -> `labels`, and — only if `messages.body_path` is
    /// set — the cached body file, run through [`feathermail_html::parse_message`]
    /// (T-093) and reduced to its selected `text/plain` part -> `body`. A
    /// message whose body is not cached yet indexes with an empty `body`
    /// column; it still matches on sender/subject/etc, and gets
    /// re-enqueued (by [`crate::body::Core::store_body`]) once the body is
    /// fetched.
    ///
    /// One SQLite transaction for the whole batch: a message is removed
    /// from `fts_pending` in the same transaction that wrote its
    /// `messages_fts` row, so a crash or an error partway through never
    /// leaves a message marked "done" without actually being searchable
    /// (or vice versa) — the next call just retries the same rows.
    ///
    /// **What this reads is parsed text, not raw cached bytes.** T-093:
    /// `messages.body_path` is the raw fetched RFC 822 message — headers,
    /// MIME boundaries, and a base64/quoted-printable-encoded body on the
    /// wire. Indexing that verbatim (as this used to) makes base64 mail
    /// unsearchable by its own words while making every message
    /// "searchable" by `Received:` chains, `Message-ID`, and `boundary=`
    /// noise instead. See [`index_one`] for exactly what gets extracted
    /// and why.
    ///
    /// This method does not schedule itself — nothing in this crate spins
    /// up a background thread or an idle callback. Driving it periodically
    /// (and only while there is something to drain) is
    /// `crates/app`/`crates/service` territory, out of bounds for this
    /// task; T-048's "does not block Inbox" artifact is satisfied
    /// structurally, by this method doing a bounded amount of work and
    /// returning, not by anything in this crate actually running it on a
    /// timer.
    pub fn index_pending_batch(
        &self,
        bodies_dir: &Path,
        limit: usize,
    ) -> Result<IndexBatchResult, CoreError> {
        let conn = self.db.conn();
        let ids: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT message_id FROM fts_pending \
                     ORDER BY queued_at ASC, message_id ASC LIMIT ?1",
                )
                .map_err(sql_err)?;
            let rows = stmt
                .query_map(params![limit as i64], |row| row.get::<_, String>(0))
                .map_err(sql_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(sql_err)?;
            rows
        };

        if ids.is_empty() {
            return Ok(IndexBatchResult {
                indexed: 0,
                remaining: 0,
            });
        }

        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        for id in &ids {
            index_one(&tx, id, bodies_dir)?;
            tx.execute("DELETE FROM fts_pending WHERE message_id = ?1", params![id])
                .map_err(sql_err)?;
        }
        tx.commit().map_err(sql_err)?;

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM fts_pending", [], |row| row.get(0))
            .map_err(sql_err)?;

        Ok(IndexBatchResult {
            indexed: ids.len(),
            remaining: remaining.max(0) as usize,
        })
    }
}

/// Turns a cached message's raw RFC 822 bytes into the text `body`
/// indexes, by reusing the exact same parser the message panel already
/// reads through (`feathermail_html::parse_message`, T-031) rather than
/// indexing the wire bytes verbatim (T-093). The index ALWAYS calls
/// `parse_message` with `prefer_plain = true`, independent of the UI
/// Privacy toggle "Prefer plain text" (T-031): search stays findable
/// by the plain part even when the reading pane is showing sanitized
/// HTML. Same charset/transfer decoding as the panel.
///
/// Three deliberate choices, all made here rather than left as "however
/// it falls out":
///
/// 1. **Headers never reach `body`.** `parse_message` splits headers from
///    the selected part's decoded text; only the latter is returned here.
///    `Received:` chains, `Message-ID`, MIME boundaries, and other header
///    text that used to leak into the raw-bytes version of this column
///    simply never enters this function's output. See
///    `search::tests::a_received_header_value_does_not_make_a_message_findable_by_it`.
/// 2. **`BodyContent::Html` (no `text/plain` alternative) indexes the
///    words it displays, never its markup** (T-096). T-093 left this arm
///    returning an empty string and wrote the gap down rather than
///    closing it, because at the time the message panel could not show
///    HTML either -- indexing text nobody could see would have been a
///    promise with no display-side counterpart. T-030 gave HTML mail a
///    real renderer, so the counterpart exists and the gap became a plain
///    bug: an HTML-only message (which is most marketing and most
///    transactional mail) was findable by sender and subject but never by
///    a word it actually contained. `feathermail_html::text_for_search`
///    is that extraction, and it lives in `crates/html` rather than here
///    for the same reason `sanitize` does: `UnsanitizedHtml` never leaves
///    that crate in raw form. Tag names, attribute values, CSS and script
///    bodies are not words the sender wrote and do not enter the index --
///    see
///    `search::tests::html_only_body_is_findable_by_its_words_and_never_by_its_markup`.
/// 3. **A body that fails to decode indexes as empty, not as an error.**
///    `BodyContent::Undecodable` (unrecognized `Content-Transfer-Encoding`)
///    and `BodyContent::Empty` both fall through to `String::new()` here,
///    matching the pre-existing "best-effort" contract this function's
///    caller already had for an unreadable cache file (see the comment at
///    its call site): one bad message must never abort or fail an
///    `index_pending_batch` call that also has good messages in it.
///
/// D14: the returned `String` is message body text and must never be
/// logged, `Debug`-printed, or embedded in an error -- it goes to exactly
/// one place, the `body` column bound in `index_one` below.
fn body_text_for_index(raw: &[u8]) -> String {
    use feathermail_html::BodyContent;

    let parsed = feathermail_html::parse_message(raw, true);
    match parsed.body {
        BodyContent::Plain(text) => text,
        BodyContent::Html(html) => feathermail_html::text_for_search(&html),
        BodyContent::Empty | BodyContent::Undecodable(_) => String::new(),
    }
}

/// (Re)index one message inside `tx`. Missing row (raced with a delete
/// since it was queued) is not an error -- the caller still removes the
/// `fts_pending` entry, there is simply nothing to write to
/// `messages_fts`.
fn index_one(
    tx: &rusqlite::Transaction<'_>,
    message_id: &str,
    bodies_dir: &Path,
) -> Result<(), CoreError> {
    let row: Option<(String, String, String, String, Option<String>)> = tx
        .query_row(
            "SELECT sender_name, sender_email, recipients, subject, body_path \
             FROM messages WHERE id = ?1",
            params![message_id],
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
        .map_err(sql_err)?;

    let Some((sender_name, sender_email, recipients, subject, body_path)) = row else {
        return Ok(());
    };

    let sender = format!("{sender_name} {sender_email}");
    let sender = sender.trim();

    let attachment_names: String = tx
        .query_row(
            "SELECT COALESCE(GROUP_CONCAT(filename, ' '), '') \
             FROM attachments WHERE message_id = ?1",
            params![message_id],
            |row| row.get(0),
        )
        .map_err(sql_err)?;

    let labels: String = tx
        .query_row(
            "SELECT COALESCE(GROUP_CONCAT(l.name, ' '), '') \
             FROM message_labels ml JOIN labels l ON l.id = ml.label_id \
             WHERE ml.message_id = ?1",
            params![message_id],
            |row| row.get(0),
        )
        .map_err(sql_err)?;

    // Best-effort: a missing/unreadable cache file, or a cached file that
    // fails to parse into anything indexable, indexes as "no body yet",
    // the same as `body_path` being NULL -- self-healing that pointer is
    // `Core::lookup_body`'s job (`crate::body`), not this method's.
    // `body_text_for_index` itself never fails (`parse_message` never
    // panics or errors -- see `feathermail_html`'s crate doc), so the only
    // failure mode left here is the file read.
    let raw = match &body_path {
        Some(rel) => std::fs::read(bodies_dir.join(rel)).ok(),
        None => None,
    };
    let body = raw.as_deref().map(body_text_for_index).unwrap_or_default();

    // T-104: the card's two preview lines, refilled from the body that is
    // already in hand.
    //
    // `messages.snippet` is written once, by `Core::store_body`, from
    // whatever the preview extractor could do *that day*. On the owner's
    // mailbox 21 of 40 cached bodies still had an empty snippet while
    // today's extractor reads 38 of those 40 -- they were cached before
    // HTML previews existed (T-096) and nothing ever went back for them, so
    // the cards stayed blank with the body sitting right there on disk.
    // Doing it here rather than in a backfill pass of its own is what makes
    // it free: this batch already read the file and already holds a write
    // transaction, and v23's migration already re-queued every message with
    // a cached body, so an existing profile heals as the queue drains.
    // Only an empty snippet is touched -- a preview that exists is never
    // rewritten by this pass.
    if let Some(bytes) = raw.as_deref() {
        refresh_empty_snippet(tx, message_id, bytes)?;
    }

    // Delete+insert rather than UPDATE: `messages_fts` is a plain
    // (non-external-content) FTS5 table. Crucially, do not address the old
    // row through its `message_id`: that FTS column is intentionally
    // UNINDEXED, so every such delete scans the full index and turns an
    // initial N-message index into O(N²). T-068's v19 migration maintains
    // the ordinary SQLite `fts_message_rows` map, so this is a direct FTS
    // rowid delete (the same lifecycle rule sync/store use when mail is
    // removed). See the module doc for why external-content triggers are
    // not an alternative here.
    tx.execute(
        "DELETE FROM messages_fts WHERE rowid IN \
         (SELECT fts_rowid FROM fts_message_rows WHERE message_id = ?1)",
        params![message_id],
    )
    .map_err(sql_err)?;
    tx.execute(
        "INSERT INTO messages_fts \
         (sender, recipients, subject, body, attachment_names, labels, message_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            sender,
            recipients,
            subject,
            body,
            attachment_names,
            labels,
            message_id
        ],
    )
    .map_err(sql_err)?;
    let fts_rowid = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO fts_message_rows (message_id, fts_rowid) VALUES (?1, ?2) \
         ON CONFLICT(message_id) DO UPDATE SET fts_rowid = excluded.fts_rowid",
        params![message_id, fts_rowid],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// T-104: fill in `messages.snippet` (and the thread's, when this message
/// is the thread's latest) if it is empty and the body yields text.
///
/// The thread update is the same statement `Core::store_body` uses, for the
/// same reason: a thread's snippet belongs to its newest message, and a
/// re-indexed older message must not overwrite it.
fn refresh_empty_snippet(
    tx: &rusqlite::Transaction<'_>,
    message_id: &str,
    raw: &[u8],
) -> Result<(), CoreError> {
    let row: Option<(String, String, String)> = tx
        .query_row(
            "SELECT account_id, thread_id, snippet FROM messages WHERE id = ?1",
            params![message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((account_id, thread_id, snippet)) = row else {
        return Ok(());
    };
    if !snippet.is_empty() {
        return Ok(());
    }
    let fresh = crate::preview::preview_from_raw_mime(raw, crate::preview::DEFAULT_PREVIEW_CHARS);
    if fresh.is_empty() {
        return Ok(());
    }
    tx.execute(
        "UPDATE messages SET snippet = ?1 WHERE id = ?2",
        params![fresh, message_id],
    )
    .map_err(sql_err)?;
    tx.execute(
        Core::UPDATE_LATEST_THREAD_SNIPPET_SQL,
        params![fresh, account_id, thread_id, message_id],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// Enqueue `message_id` for (re)indexing. `INSERT OR IGNORE`: if it is
/// already pending, leave it be -- whenever `index_one` eventually runs
/// for it, it reads `messages`' state as of *that* moment, so there is
/// nothing to lose by not bumping `queued_at`.
///
/// Called from two places, matching the module doc's two cases: the
/// new-message branch of `CoreSyncStore::upsert_one`
/// (`crates/core/src/sync_store.rs`) and `Core::store_body`
/// (`crates/core/src/body.rs`).
pub(crate) fn enqueue_for_indexing(
    conn: &rusqlite::Connection,
    message_id: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO fts_pending (message_id, queued_at) \
         VALUES (?1, strftime('%s','now'))",
        params![message_id],
    )?;
    Ok(())
}

/// `Date{year, month, day}` (UTC midnight) -> Unix seconds. `messages.date`
/// is stored as Unix seconds UTC (see `crates/db/src/schema.sql`'s doc on
/// `threads.date`/the T-024 `body_bytes` comment referencing it), and
/// `feathermail_search::Date` deliberately carries no timezone (see its
/// doc comment: "the caller ... is responsible for turning this into
/// whatever bound its storage layer needs"). Nothing in this workspace
/// stores a user timezone yet (checked: no such column/setting exists),
/// so this uses UTC for both the message clock and the day boundary --
/// consistent with each other, but potentially off by up to a day from
/// what a non-UTC user meant by "today" when they typed `after:`/
/// `before:`. Named here rather than silently assumed: fixing it needs a
/// timezone source this crate does not have.
fn epoch_seconds_for_date(d: &Date) -> i64 {
    days_from_civil(i64::from(d.year), i64::from(d.month), i64::from(d.day)) * 86_400
}

/// Howard Hinnant's `days_from_civil`: proleptic Gregorian calendar date ->
/// days since the Unix epoch (1970-01-01 -> 0). Pure integer arithmetic,
/// no external date/time crate needed for what is otherwise this method's
/// only piece of date math.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_search::Query;
    use rusqlite::params;

    fn seed_account(core: &Core, account: &str, email: &str) {
        core.db
            .conn()
            .execute(
                "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at) \
                 VALUES (?1, ?1, ?2, 'generic', 'synced', 'recent', 0, 0)",
                params![account, email],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, name, kind) VALUES (?1, ?2, 'Inbox', 'inbox')",
                params![format!("{account}:inbox"), account],
            )
            .unwrap();
    }

    /// Mirrors `CoreSyncStore::upsert_one`'s new-message branch closely
    /// enough for these tests: a `threads` row, a `messages` row, and (the
    /// point of this whole module) the `fts_pending` enqueue that
    /// production code does via `enqueue_for_indexing`.
    #[allow(clippy::too_many_arguments)]
    fn seed_message(
        core: &Core,
        account: &str,
        msg_id: &str,
        subject: &str,
        sender_name: &str,
        sender_email: &str,
        recipients: &str,
        date: i64,
        unread: bool,
        starred: bool,
        has_attachment: bool,
    ) {
        let inbox = format!("{account}:inbox");
        let thread = format!("{account}:t:{msg_id}");
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred) \
             VALUES (?1, ?2, ?3, ?4, '', ?5, ?6, ?7)",
            params![thread, account, inbox, subject, date, unread as i64, starred as i64],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, account_id, thread_id, folder_id, date, sender_name, \
             sender_email, recipients, subject, unread, starred, has_attachment) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                msg_id,
                account,
                thread,
                inbox,
                date,
                sender_name,
                sender_email,
                recipients,
                subject,
                unread as i64,
                starred as i64,
                has_attachment as i64,
            ],
        )
        .unwrap();
        enqueue_for_indexing(conn, msg_id).unwrap();
    }

    fn plan(query: &str) -> SearchPlan {
        Query::parse(query).to_search_plan()
    }

    // --- indexing (case 1: a message appeared) ---

    #[test]
    fn a_newly_synced_message_is_unsearchable_until_indexed_then_searchable_after() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "Quarterly report",
            "Alice",
            "alice@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();

        let before = core
            .search(&AccountId("acc1".into()), &plan("quarterly"), None, 0)
            .unwrap();
        assert!(
            before.threads.is_empty(),
            "not indexed yet -- must not be findable"
        );
        assert_eq!(before.pending_index, 1);

        let batch = core.index_pending_batch(dir.path(), 10).unwrap();
        assert_eq!(batch.indexed, 1);
        assert_eq!(batch.remaining, 0);

        let after = core
            .search(&AccountId("acc1".into()), &plan("quarterly"), None, 0)
            .unwrap();
        assert_eq!(after.threads.len(), 1);
        assert_eq!(after.threads[0].id.as_str(), "acc1:t:m1");
        assert_eq!(after.pending_index, 0);
    }

    /// T-068: `message_id` is intentionally UNINDEXED inside FTS5. The
    /// normal SQLite map must therefore be both updated on a reindex and
    /// used as the direct lookup path for the old FTS row; otherwise a large
    /// initial index quietly becomes a full virtual-table scan per message.
    #[test]
    fn reindex_replaces_the_fts_row_through_its_mapped_rowid() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "First subject",
            "Alice",
            "alice@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();
        let conn = core.db.conn();
        conn.execute(
            "UPDATE messages SET subject = 'Second subject' WHERE id = 'm1'",
            [],
        )
        .unwrap();
        enqueue_for_indexing(conn, "m1").unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let second_rowid: i64 = conn
            .query_row(
                "SELECT fts_rowid FROM fts_message_rows WHERE message_id = 'm1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let fts_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE message_id = 'm1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            fts_rows, 1,
            "a reindex must replace, not duplicate, the FTS row"
        );
        let stored_rowid: i64 = conn
            .query_row(
                "SELECT rowid FROM messages_fts WHERE message_id = 'm1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            second_rowid, stored_rowid,
            "the map must follow the newly written FTS row (FTS may reuse a rowid)"
        );
        let old_subject = core
            .search(&AccountId("acc1".into()), &plan("first"), None, 10)
            .unwrap();
        let new_subject = core
            .search(&AccountId("acc1".into()), &plan("second"), None, 10)
            .unwrap();
        assert!(old_subject.threads.is_empty());
        assert_eq!(new_subject.threads.len(), 1);

        let plan = conn
            .prepare(
                "EXPLAIN QUERY PLAN DELETE FROM messages_fts WHERE rowid IN \
                 (SELECT fts_rowid FROM fts_message_rows WHERE message_id = ?1)",
            )
            .unwrap()
            .query_map(params!["m1"], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|detail| detail.contains("sqlite_autoindex_fts_message_rows_1")),
            "the mapped FTS delete must seek the map primary key, plan was {plan:?}"
        );
    }

    #[test]
    fn partial_index_still_answers_for_what_is_already_indexed() {
        // T-048's own acceptance line: "во время index поиск работает по
        // уже проиндексированному". Two messages queued, only one drained
        // (limit = 1) -- the indexed one must be findable and the
        // unindexed one must not be silently guessed at, and the caller
        // must be able to see the index is not finished.
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "Alpha",
            "A",
            "a@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        seed_message(
            &core,
            "acc1",
            "m2",
            "Beta",
            "B",
            "b@example.com",
            "",
            200,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();

        let batch = core.index_pending_batch(dir.path(), 1).unwrap();
        assert_eq!(batch.indexed, 1);
        assert_eq!(batch.remaining, 1);

        let results = core
            .search(&AccountId("acc1".into()), &plan("alpha OR beta"), None, 0)
            .unwrap();
        // "OR" is literal text per D54 (T-047), so this really is two
        // separate literal terms ANDed together and matches neither by
        // itself -- switch to independent single-word searches instead.
        assert!(results.threads.is_empty());

        let alpha = core
            .search(&AccountId("acc1".into()), &plan("alpha"), None, 0)
            .unwrap();
        assert_eq!(alpha.pending_index, 1, "one message is still unindexed");
        // Whichever of the two got the batch's one slot is indexed; find
        // out which without assuming an order `fts_pending`'s draining
        // doesn't promise beyond `queued_at ASC`.
        let indexed_alpha = !alpha.threads.is_empty();
        let beta = core
            .search(&AccountId("acc1".into()), &plan("beta"), None, 0)
            .unwrap();
        let indexed_beta = !beta.threads.is_empty();
        assert_ne!(
            indexed_alpha, indexed_beta,
            "exactly one of the two messages was indexed by a batch of size 1"
        );
    }

    // --- indexing (case 2: a body arrived later) ---

    #[test]
    fn body_text_becomes_searchable_only_after_it_is_cached_and_reindexed() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "No subject match",
            "A",
            "a@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let before = core
            .search(&AccountId("acc1".into()), &plan("unicorn"), None, 0)
            .unwrap();
        assert!(before.threads.is_empty(), "body not cached yet");

        let mut core = core;
        core.store_body(
            &crate::model::MessageId("m1".into()),
            dir.path(),
            b"the body mentions a unicorn",
        )
        .unwrap();
        // store_body must have re-enqueued m1 -- it does not write
        // messages_fts itself.
        let mid_search = core
            .search(&AccountId("acc1".into()), &plan("unicorn"), None, 0)
            .unwrap();
        assert!(
            mid_search.threads.is_empty(),
            "body cached but not reindexed yet must still not match"
        );
        assert_eq!(mid_search.pending_index, 1);

        core.index_pending_batch(dir.path(), 10).unwrap();
        let after = core
            .search(&AccountId("acc1".into()), &plan("unicorn"), None, 0)
            .unwrap();
        assert_eq!(after.threads.len(), 1, "body text is now indexed");
    }

    #[test]
    fn reindexing_after_body_arrives_replaces_not_duplicates_the_fts_row() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "Subject",
            "A",
            "a@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let mut core = core;
        core.store_body(&crate::model::MessageId("m1".into()), dir.path(), b"hello")
            .unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let count: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE message_id = 'm1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "delete+insert must not leave a duplicate row");
    }

    // --- T-093: `body` holds parsed text, not raw RFC 822 bytes ---

    /// Seeds one message and caches `raw` (real RFC 822 bytes, headers
    /// included) as its body via `store_body`, then drains the index --
    /// the same two-step "appear, then body arrives" path production code
    /// goes through, exercised here in one helper since every test below
    /// only cares about the end state.
    fn seed_and_index_raw_body(core: &mut Core, dir: &std::path::Path, msg_id: &str, raw: &[u8]) {
        seed_account(core, "acc1", "me@example.com");
        seed_message(
            core,
            "acc1",
            msg_id,
            "Subject line unrelated to the body",
            "Sender",
            "sender@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        core.index_pending_batch(dir, 10).unwrap();
        core.store_body(&crate::model::MessageId(msg_id.into()), dir, raw)
            .unwrap();
        core.index_pending_batch(dir, 10).unwrap();
    }

    /// T-104 (владелец: «грузить и кешировать 100 последних, чтобы на
    /// карточках была вся информация»): a body cached by an older build kept
    /// an empty snippet for ever -- the card stayed blank while the body sat
    /// on disk. The indexer already reads that file, so it fills the gap.
    ///
    /// Mutation: drop the `refresh_empty_snippet` call -> the snippet stays
    /// empty and this fails.
    #[test]
    fn indexing_refills_a_snippet_an_older_build_left_empty() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        let raw = b"Content-Type: text/plain; charset=utf-8\r\n\r\nHello from the cached body";
        seed_and_index_raw_body(&mut core, dir.path(), "m1", raw);

        // Exactly the state a pre-T-096 profile is in: body on disk, snippet
        // blank on the message and on its thread.
        core.db
            .conn()
            .execute("UPDATE messages SET snippet = '' WHERE id = 'm1'", [])
            .unwrap();
        core.db
            .conn()
            .execute("UPDATE threads SET snippet = ''", [])
            .unwrap();
        enqueue_for_indexing(core.db.conn(), "m1").unwrap();

        core.index_pending_batch(dir.path(), 10).unwrap();

        let message: String = core
            .db
            .conn()
            .query_row("SELECT snippet FROM messages WHERE id='m1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let thread: String = core
            .db
            .conn()
            .query_row("SELECT snippet FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(message, "Hello from the cached body");
        assert_eq!(
            thread, "Hello from the cached body",
            "the card reads this one"
        );
    }

    /// A preview that already exists is never rewritten by the index pass:
    /// re-indexing is not an excuse to churn what the list is showing.
    #[test]
    fn indexing_leaves_an_existing_snippet_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = Core::memory().unwrap();
        let raw = b"Content-Type: text/plain; charset=utf-8\r\n\r\nHello from the cached body";
        seed_and_index_raw_body(&mut core, dir.path(), "m1", raw);
        core.db
            .conn()
            .execute(
                "UPDATE messages SET snippet = 'hand-written' WHERE id='m1'",
                [],
            )
            .unwrap();
        enqueue_for_indexing(core.db.conn(), "m1").unwrap();

        core.index_pending_batch(dir.path(), 10).unwrap();

        let message: String = core
            .db
            .conn()
            .query_row("SELECT snippet FROM messages WHERE id='m1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(message, "hand-written");
    }

    #[test]
    fn quoted_printable_cyrillic_body_is_findable_by_a_word_from_the_body() {
        // "привет" (UTF-8), quoted-printable-encoded byte by byte.
        let raw = b"Content-Type: text/plain; charset=utf-8\r\n\
             Content-Transfer-Encoding: quoted-printable\r\n\
             \r\n\
             =D0=BF=D1=80=D0=B8=D0=B2=D0=B5=D1=82";
        let mut core = Core::memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_and_index_raw_body(&mut core, dir.path(), "m1", raw);

        let results = core
            .search(&AccountId("acc1".into()), &plan("привет"), None, 0)
            .unwrap();
        assert_eq!(
            results.threads.len(),
            1,
            "quoted-printable cyrillic body text must be findable by its own word"
        );
    }

    #[test]
    fn windows_1251_body_is_findable_by_a_word_from_the_body() {
        // "привет" in cp1251 (same bytes as
        // `feathermail_html::charset::tests::windows_1251_decodes_privet`).
        let mut raw = b"Content-Type: text/plain; charset=windows-1251\r\n\r\n".to_vec();
        raw.extend_from_slice(&[0xEF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]);
        let mut core = Core::memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_and_index_raw_body(&mut core, dir.path(), "m1", &raw);

        let results = core
            .search(&AccountId("acc1".into()), &plan("привет"), None, 0)
            .unwrap();
        assert_eq!(
            results.threads.len(),
            1,
            "windows-1251 body text must be findable by its own word"
        );
    }

    #[test]
    fn base64_body_is_findable_by_a_word_from_the_body() {
        // base64 for "the body mentions a unicorn".
        let raw = b"Content-Type: text/plain\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             dGhlIGJvZHkgbWVudGlvbnMgYSB1bmljb3Ju";
        let mut core = Core::memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_and_index_raw_body(&mut core, dir.path(), "m1", raw);

        let results = core
            .search(&AccountId("acc1".into()), &plan("unicorn"), None, 0)
            .unwrap();
        assert_eq!(
            results.threads.len(),
            1,
            "the main practical case: base64 mail must be findable by a word from its body"
        );
    }

    #[test]
    fn a_received_header_value_does_not_make_a_message_findable_by_it() {
        // The exact regression T-093 exists for: the raw-bytes version of
        // `index_one` put whole headers into `body`, so a message became
        // "findable" by `Received:` chain junk that has nothing to do with
        // what it is about. `GALAXYQUEST` appears only inside the
        // `Received:` header, never in the actual body text.
        let raw = b"Received: from mail.example.com by GALAXYQUEST-relay-07 with ESMTP\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             an ordinary plain-text body, nothing special here";
        let mut core = Core::memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_and_index_raw_body(&mut core, dir.path(), "m1", raw);

        let by_header = core
            .search(&AccountId("acc1".into()), &plan("galaxyquest"), None, 0)
            .unwrap();
        assert!(
            by_header.threads.is_empty(),
            "a Received: header value must not make the message findable by it"
        );

        // Sanity check that the message *is* indexed at all -- otherwise
        // the assertion above would trivially pass for the wrong reason
        // (nothing indexed, rather than headers correctly excluded).
        let by_body = core
            .search(&AccountId("acc1".into()), &plan("ordinary"), None, 0)
            .unwrap();
        assert_eq!(
            by_body.threads.len(),
            1,
            "the real body text must still be indexed"
        );
    }

    #[test]
    fn multipart_alternative_prefers_the_plain_part_not_whatever_parse_message_defaults_to() {
        // T-093 review finding: `body_text_for_index` hardcodes
        // `prefer_plain = true` in its call to `parse_message`, but
        // nothing pinned *that specific argument* -- flipping it to
        // `false` (picking `text/html` instead) still made every other
        // T-093 test pass, because none of them exercised a message that
        // actually offers both parts. A real `multipart/alternative`
        // message (the common shape for most mail clients) does exactly
        // that, and this is the case that regresses hardest if the
        // argument ever drifts: `text/html` gets chosen, the message is
        // indexed from the wrong half of itself, and every difference
        // between the two halves silently becomes a search miss.
        //
        // Two words, one per alternative, so a single assertion can't
        // pass for the wrong reason: `papyrus` only exists in the
        // `text/plain` part (proves plain is the one actually selected —
        // catches `true` -> `false`), `spreadsheet` only exists inside a
        // `<table>` in the `text/html` part. Since T-096 that text would
        // be indexed if it were the selected part, so the second half is
        // now a direct test that alternative selection happens before
        // extraction -- not, as it was under T-093, a side effect of an
        // HTML body indexing as empty no matter which part won.
        let raw: &[u8] = b"Content-Type: multipart/alternative; boundary=B\r\n\r\n--B\r\nContent-Type: text/plain\r\n\r\nthe papyrus scroll arrived\r\n--B\r\nContent-Type: text/html\r\n\r\n<table><tr><td>spreadsheet</td></tr></table>\r\n--B--\r\n";
        let mut core = Core::memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_and_index_raw_body(&mut core, dir.path(), "m1", raw);

        let by_plain_word = core
            .search(&AccountId("acc1".into()), &plan("papyrus"), None, 0)
            .unwrap();
        assert_eq!(
            by_plain_word.threads.len(),
            1,
            "the text/plain alternative must be the one selected and indexed"
        );

        let by_html_word = core
            .search(&AccountId("acc1".into()), &plan("spreadsheet"), None, 0)
            .unwrap();
        assert!(
            by_html_word.threads.is_empty(),
            "the unselected text/html alternative's text must not leak into the index"
        );

        let by_tag = core
            .search(&AccountId("acc1".into()), &plan("table"), None, 0)
            .unwrap();
        assert!(
            by_tag.threads.is_empty(),
            "the unselected text/html alternative's markup must not leak into the index either"
        );
    }

    #[test]
    fn html_only_body_is_findable_by_its_words_and_never_by_its_markup() {
        // T-096: an HTML-only message -- no `text/plain` alternative,
        // which is most marketing and most transactional mail -- indexes
        // the words it displays. Both halves matter and this test pins
        // both: the words are there, and nothing that is markup rather
        // than message is (a search for the tag name `table`, for the CSS
        // property `font-family`, for a class name, or for a tracking
        // parameter out of an `href` must not match). Under T-093 this
        // test existed as the safety half alone, with the body indexing
        // empty; the first loop is what T-030's real renderer made it
        // honest to add.
        let raw = b"Content-Type: text/html; charset=utf-8\r\n\
             \r\n\
             <html><head><style>.summary { font-family: Helvetica }</style></head>\
             <body><table class=\"summary\"><tr><td>invoice</td><td>overdue</td></tr></table>\
             <a href=\"https://t.example/c?campaign=quarterly\">Pay now</a></body></html>";
        let mut core = Core::memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        seed_and_index_raw_body(&mut core, dir.path(), "m1", raw);

        for word in ["invoice", "overdue", "Pay"] {
            let hits = core
                .search(&AccountId("acc1".into()), &plan(word), None, 0)
                .unwrap();
            assert_eq!(
                hits.threads.len(),
                1,
                "an HTML-only body must be findable by the word `{word}` it displays"
            );
        }

        // Adjacent table cells must not weld into one token nobody can
        // ever search for -- and a table is the layout element of
        // virtually all HTML mail.
        let welded = core
            .search(&AccountId("acc1".into()), &plan("invoiceoverdue"), None, 0)
            .unwrap();
        assert!(
            welded.threads.is_empty(),
            "cell boundaries must separate words, not glue them"
        );

        for markup in ["table", "font-family", "Helvetica", "summary", "campaign"] {
            let hits = core
                .search(&AccountId("acc1".into()), &plan(markup), None, 0)
                .unwrap();
            assert!(
                hits.threads.is_empty(),
                "`{markup}` is markup, not message text, and must not be indexed"
            );
        }

        // Non-body columns are unaffected.
        let by_subject = core
            .search(&AccountId("acc1".into()), &plan("unrelated"), None, 0)
            .unwrap();
        assert_eq!(by_subject.threads.len(), 1);
    }

    #[test]
    fn an_unparseable_cached_body_does_not_fail_the_rest_of_the_batch() {
        // Two messages queued for the same batch: m1's cached "body" is
        // garbage (not even RFC 822 headers, arbitrary bytes that could
        // trip up a naive parser), m2's is a normal plain-text body.
        // `feathermail_html::parse_message` never panics or errors by its
        // own contract, so this is really pinning "best-effort survives
        // the worst input, not just I/O errors" -- the pre-existing
        // contract this function already had for a missing/unreadable
        // cache file, now extended to cover a cache file that exists but
        // makes no sense as mail.
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "First",
            "A",
            "a@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        seed_message(
            &core,
            "acc1",
            "m2",
            "Second",
            "B",
            "b@example.com",
            "",
            200,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let mut core = core;
        let garbage: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        core.store_body(&crate::model::MessageId("m1".into()), dir.path(), &garbage)
            .unwrap();
        core.store_body(
            &crate::model::MessageId("m2".into()),
            dir.path(),
            b"Content-Type: text/plain\r\n\r\nfindable body text",
        )
        .unwrap();

        let batch = core.index_pending_batch(dir.path(), 10).unwrap();
        assert_eq!(
            batch.indexed, 2,
            "the batch must finish both rows, not abort on the unparseable one"
        );
        assert_eq!(batch.remaining, 0);

        let m2_found = core
            .search(&AccountId("acc1".into()), &plan("findable"), None, 0)
            .unwrap();
        assert_eq!(
            m2_found.threads.len(),
            1,
            "m2's real body text must still be indexed after m1's garbage body"
        );
    }

    #[test]
    fn an_unreadable_cached_body_file_does_not_fail_the_rest_of_the_batch() {
        // The other half of "unreadable" -- not garbage content, but a
        // `body_path` that points at a file the filesystem can no longer
        // hand back (deleted out from under `messages.body_path`, e.g. by
        // a cache eviction race). This is the pre-existing best-effort
        // contract `index_one` already had before T-093 -- pinned here so
        // T-093's rewrite of that line cannot regress it silently.
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "First",
            "A",
            "a@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        seed_message(
            &core,
            "acc1",
            "m2",
            "Second",
            "B",
            "b@example.com",
            "",
            200,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let mut core = core;
        core.store_body(
            &crate::model::MessageId("m1".into()),
            dir.path(),
            b"will be deleted",
        )
        .unwrap();
        core.store_body(
            &crate::model::MessageId("m2".into()),
            dir.path(),
            b"Content-Type: text/plain\r\n\r\nfindable body text",
        )
        .unwrap();

        // Delete m1's cache file out from under its `body_path` row, so
        // `index_one`'s `std::fs::read` fails for it specifically.
        let m1_body_path: String = core
            .db
            .conn()
            .query_row(
                "SELECT body_path FROM messages WHERE id = 'm1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        std::fs::remove_file(dir.path().join(m1_body_path)).unwrap();

        let batch = core.index_pending_batch(dir.path(), 10).unwrap();
        assert_eq!(
            batch.indexed, 2,
            "the batch must finish both rows, not abort because one body file vanished"
        );
        assert_eq!(batch.remaining, 0);

        let m2_found = core
            .search(&AccountId("acc1".into()), &plan("findable"), None, 0)
            .unwrap();
        assert_eq!(
            m2_found.threads.len(),
            1,
            "m2's real body text must still be indexed after m1's cache file vanished"
        );
    }

    // --- account isolation (critical) ---

    #[test]
    fn search_never_returns_another_accounts_message() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "one@example.com");
        seed_account(&core, "acc2", "two@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "Confidential contract terms",
            "A",
            "a@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        seed_message(
            &core,
            "acc2",
            "m2",
            "Confidential contract terms",
            "B",
            "b@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let acc1_results = core
            .search(&AccountId("acc1".into()), &plan("confidential"), None, 0)
            .unwrap();
        assert_eq!(acc1_results.threads.len(), 1);
        assert_eq!(acc1_results.threads[0].id.as_str(), "acc1:t:m1");

        let acc2_results = core
            .search(&AccountId("acc2".into()), &plan("confidential"), None, 0)
            .unwrap();
        assert_eq!(acc2_results.threads.len(), 1);
        assert_eq!(acc2_results.threads[0].id.as_str(), "acc2:t:m2");
    }

    /// Pins the defense-in-depth called out in the module doc, against
    /// the exact state it exists for: a `messages` row whose `account_id`
    /// disagrees with its own thread's `account_id`. Production code
    /// never produces this row -- `CoreSyncStore::thread_row_id` bakes the
    /// account into the thread id (`"thr:{account}:{folder}:{uid}"`), so
    /// a message and its thread always agree today. That is exactly why
    /// this test builds the row directly instead of going through
    /// `seed_message`: the invariant this defense protects is not one the
    /// current write path can violate, but T-029 (real `References`/
    /// `In-Reply-To` threading) will very likely replace `thread_row_id`
    /// with something that no longer bakes the account in, at which point
    /// this stops being a hypothetical.
    ///
    /// `t1` belongs to `acc1`. `m1` (account `acc1`) is `t1`'s real
    /// message. `m_leak` is inserted directly against `t1` but claims
    /// `account_id = "acc2"` and carries text nothing else in the fixture
    /// contains. A search by `acc1` (the thread's rightful owner) for
    /// that text must not surface `t1` -- if it does, `acc1` has learned
    /// that a message matching their query exists in their own thread,
    /// when that message does not actually belong to them.
    #[test]
    fn search_never_returns_a_thread_whose_only_matching_message_belongs_to_a_different_account() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "one@example.com");
        seed_account(&core, "acc2", "two@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "Team sync notes",
            "A",
            "a@example.com",
            "",
            100,
            false,
            false,
            false,
        );

        // Bypasses `seed_message` on purpose (see the doc comment above):
        // `m_leak.account_id` disagrees with `t1`'s own `account_id`,
        // a state today's write path cannot produce.
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO messages (id, account_id, thread_id, folder_id, date, sender_name, \
             sender_email, recipients, subject, unread, starred, has_attachment) \
             VALUES ('m_leak', 'acc2', 'acc1:t:m1', 'acc1:inbox', 100, 'Eve', \
             'eve@example.com', '', 'unrelated subject', 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages_fts (sender, recipients, subject, body, attachment_names, \
             labels, message_id) VALUES ('Eve eve@example.com', '', 'unrelated subject', \
             'topsecret-acc2-payload', '', '', 'm_leak')",
            [],
        )
        .unwrap();

        let results = core
            .search(
                &AccountId("acc1".into()),
                &plan("topsecret-acc2-payload"),
                None,
                0,
            )
            .unwrap();
        assert_eq!(
            results.threads.len(),
            0,
            "a thread must not surface for acc1 on a match that only comes from a message \
             belonging to acc2, even though the thread itself is acc1's"
        );
    }

    #[test]
    fn search_unknown_account_is_account_not_found() {
        let core = Core::memory().unwrap();
        let err = core
            .search(&AccountId("ghost".into()), &plan("anything"), None, 0)
            .unwrap_err();
        assert_eq!(err.code, crate::error::ErrorCode::AccountNotFound);
    }

    // --- predicate execution ---

    #[test]
    fn is_unread_predicate_filters_out_read_messages() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "Report",
            "A",
            "a@example.com",
            "",
            100,
            true,
            false,
            false,
        );
        seed_message(
            &core,
            "acc1",
            "m2",
            "Report",
            "B",
            "b@example.com",
            "",
            200,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let results = core
            .search(
                &AccountId("acc1".into()),
                &plan("report is:unread"),
                None,
                0,
            )
            .unwrap();
        assert_eq!(results.threads.len(), 1);
        assert_eq!(results.threads[0].id.as_str(), "acc1:t:m1");
    }

    #[test]
    fn has_attachment_predicate_filters_out_messages_without_one() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "Files",
            "A",
            "a@example.com",
            "",
            100,
            false,
            false,
            true,
        );
        seed_message(
            &core,
            "acc1",
            "m2",
            "Files",
            "B",
            "b@example.com",
            "",
            200,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let results = core
            .search(
                &AccountId("acc1".into()),
                &plan("files has:attachment"),
                None,
                0,
            )
            .unwrap();
        assert_eq!(results.threads.len(), 1);
        assert_eq!(results.threads[0].id.as_str(), "acc1:t:m1");
    }

    #[test]
    fn to_me_resolves_against_the_signed_in_accounts_own_address() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "For me",
            "A",
            "a@example.com",
            "me@example.com",
            100,
            false,
            false,
            false,
        );
        seed_message(
            &core,
            "acc1",
            "m2",
            "Not for me",
            "A",
            "a@example.com",
            "someone-else@example.com",
            200,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let results = core
            .search(&AccountId("acc1".into()), &plan("to:me"), None, 0)
            .unwrap();
        assert_eq!(results.threads.len(), 1);
        assert_eq!(results.threads[0].id.as_str(), "acc1:t:m1");
    }

    #[test]
    fn after_before_date_predicates_bound_by_utc_midnight() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        // 2026-08-01 00:00:00 UTC and 2026-09-01 00:00:00 UTC.
        let aug_1 = epoch_seconds_for_date(&Date {
            year: 2026,
            month: 8,
            day: 1,
        });
        let sep_1 = epoch_seconds_for_date(&Date {
            year: 2026,
            month: 9,
            day: 1,
        });
        seed_message(
            &core,
            "acc1",
            "in_range",
            "Report",
            "A",
            "a@example.com",
            "",
            aug_1,
            false,
            false,
            false,
        );
        seed_message(
            &core,
            "acc1",
            "on_boundary",
            "Report",
            "A",
            "a@example.com",
            "",
            sep_1,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let results = core
            .search(
                &AccountId("acc1".into()),
                &plan("report after:2026-08-01 before:2026-09-01"),
                None,
                0,
            )
            .unwrap();
        assert_eq!(results.threads.len(), 1);
        assert_eq!(results.threads[0].id.as_str(), "acc1:t:in_range");
    }

    #[test]
    fn epoch_seconds_for_date_matches_known_unix_epoch() {
        assert_eq!(
            epoch_seconds_for_date(&Date {
                year: 1970,
                month: 1,
                day: 1
            }),
            0
        );
        // 2026-08-22T00:00:00Z, cross-checked against `date -u -d
        // 2026-08-22 +%s`.
        assert_eq!(
            epoch_seconds_for_date(&Date {
                year: 2026,
                month: 8,
                day: 22
            }),
            1_787_356_800
        );
    }

    // --- empty query / no matches ---

    #[test]
    fn empty_query_returns_every_indexed_thread_for_the_account() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "One",
            "A",
            "a@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let results = core
            .search(&AccountId("acc1".into()), &plan(""), None, 0)
            .unwrap();
        assert_eq!(results.threads.len(), 1);
    }

    // --- pagination (T-049 г) ---

    #[test]
    fn search_pagination_walks_every_match_exactly_once_across_pages() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        for (i, date) in [500, 400, 300, 200, 100].into_iter().enumerate() {
            seed_message(
                &core,
                "acc1",
                &format!("m{i}"),
                "Page",
                "A",
                "a@example.com",
                "",
                date,
                false,
                false,
                false,
            );
        }
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let account = AccountId("acc1".into());
        let q = plan("page");

        let page1 = core.search(&account, &q, None, 2).unwrap();
        assert_eq!(page1.threads.len(), 2);
        assert!(
            page1.next.is_some(),
            "3 more matches remain after this page, must offer a next cursor"
        );

        let page2 = core.search(&account, &q, page1.next.as_ref(), 2).unwrap();
        assert_eq!(page2.threads.len(), 2);
        assert!(page2.next.is_some());

        let page3 = core.search(&account, &q, page2.next.as_ref(), 2).unwrap();
        assert_eq!(page3.threads.len(), 1);
        assert!(
            page3.next.is_none(),
            "exactly 5 matches total, the third page is the last"
        );

        let mut seen: Vec<String> = page1
            .threads
            .iter()
            .chain(page2.threads.iter())
            .chain(page3.threads.iter())
            .map(|t| t.id.as_str().to_string())
            .collect();
        seen.sort();
        let mut expected: Vec<String> = (0..5).map(|i| format!("acc1:t:m{i}")).collect();
        expected.sort();
        assert_eq!(
            seen, expected,
            "pagination must cover every match exactly once, no gaps or repeats"
        );
    }

    #[test]
    fn search_next_cursor_is_none_when_the_first_page_already_holds_every_match() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_message(
            &core,
            "acc1",
            "m1",
            "Solo",
            "A",
            "a@example.com",
            "",
            100,
            false,
            false,
            false,
        );
        let dir = tempfile::tempdir().unwrap();
        core.index_pending_batch(dir.path(), 10).unwrap();

        let results = core
            .search(&AccountId("acc1".into()), &plan("solo"), None, 50)
            .unwrap();
        assert_eq!(results.threads.len(), 1);
        assert!(
            results.next.is_none(),
            "one match, well under the page size -- there is no next page"
        );
    }

    // --- search history (T-049 д) ---

    #[test]
    fn search_history_round_trips_newest_first_and_deduplicates() {
        let core = Core::memory().unwrap();
        core.record_search_history("invoice").unwrap();
        core.record_search_history("from:alice").unwrap();
        // Same query again -- must not push "invoice" out of view twice.
        core.record_search_history("invoice").unwrap();

        let history = core.list_search_history(0).unwrap();
        assert_eq!(
            history,
            vec!["invoice".to_string(), "from:alice".to_string()],
            "newest distinct query first, no duplicate entries"
        );
    }

    #[test]
    fn search_history_ignores_blank_queries() {
        let core = Core::memory().unwrap();
        core.record_search_history("   ").unwrap();
        core.record_search_history("").unwrap();
        assert!(core.list_search_history(0).unwrap().is_empty());
    }

    #[test]
    fn search_history_limit_caps_the_number_returned() {
        let core = Core::memory().unwrap();
        for i in 0..5 {
            core.record_search_history(&format!("q{i}")).unwrap();
        }
        let history = core.list_search_history(2).unwrap();
        assert_eq!(history, vec!["q4".to_string(), "q3".to_string()]);
    }

    // --- index usage (T-049 з, deterministic half) ---
    //
    // Mirrors `crates/db/src/lib.rs`'s
    // `inbox_page_uses_account_folder_date_index`: read back
    // `EXPLAIN QUERY PLAN`, not stopwatch timing, for the claim that must
    // hold on every run regardless of machine speed -- "this query goes
    // through an index, not a full table/FTS scan". The bench below
    // covers the separate, inherently-noisy "and it's fast" claim.

    #[test]
    fn search_with_a_free_text_term_reaches_the_fts5_index_not_a_full_scan() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        let query_plan = Query::parse("invoice").to_search_plan();
        let (sql, binds) = core
            .build_search_sql(&AccountId("acc1".into()), &query_plan, None, 50)
            .unwrap();
        let p: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let explain = core.db.explain_query_plan(&sql, &p).unwrap();
        let lower = explain.to_ascii_lowercase();
        assert!(
            lower.contains("virtual table"),
            "a free-text term must be answered by fts5's own index (an fts5 \
            VIRTUAL TABLE scan is an indexed lookup through its inverted \
            index, despite SQLite's plan wording), plan was:\n{explain}"
        );
        assert!(
            lower.contains("threads_account_date"),
            "a broad FTS page must stay in account/date order without sorting every \
             match, plan was:\n{explain}"
        );
        assert!(
            !lower.contains("scan m ") && !lower.contains("scan t "),
            "messages/threads must be reached by an index lookup \
             (SEARCH ... USING INDEX), never a full table SCAN, plan was:\n{explain}"
        );
    }

    #[test]
    fn search_with_only_predicates_still_uses_an_index_not_a_full_table_scan() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        // No free-text term at all -- nothing for fts5 to help with, so
        // this is the case most tempted to fall back to a full scan.
        let query_plan = Query::parse("is:unread").to_search_plan();
        let (sql, binds) = core
            .build_search_sql(&AccountId("acc1".into()), &query_plan, None, 50)
            .unwrap();
        let p: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let explain = core.db.explain_query_plan(&sql, &p).unwrap();
        let lower = explain.to_ascii_lowercase();
        assert!(
            lower.contains("messages_unread"),
            "an is:unread-only query must still reach the messages_unread \
             partial index, plan was:\n{explain}"
        );
        assert!(
            !lower.contains("scan m ") && !lower.contains("scan t "),
            "plan was:\n{explain}"
        );
    }

    // --- bench (T-049 з: < 100ms on 10k) ---

    /// Seed `n` already-indexed messages directly -- one shared
    /// transaction instead of `seed_message`'s one-autocommit-per-row --
    /// so building this fixture is not itself what a flaky run's timing
    /// budget gets spent on. Writes `messages_fts` directly (bypassing
    /// `fts_pending`/`index_pending_batch`, already covered by the
    /// indexing tests above): this bench isolates `Core::search`'s own
    /// query cost against an already-caught-up index, not the separate
    /// question of how fast the indexer drains a backlog.
    fn seed_bulk_indexed(core: &Core, account: &str, n: usize) {
        let inbox = format!("{account}:inbox");
        let conn = core.db.conn();
        let tx = conn.unchecked_transaction().unwrap();
        for i in 0..n {
            let thread = format!("{account}:t:m{i}");
            let msg = format!("m{i}");
            // Most messages are ordinary, a scattered few carry the word
            // the bench searches for -- a real search has some matches,
            // not zero, and not "every row matches" either.
            let subject = if i % 137 == 0 {
                "Unicorn report"
            } else {
                "Ordinary mail"
            };
            tx.execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred) \
                 VALUES (?1, ?2, ?3, ?4, '', ?5, 0, 0)",
                params![thread, account, inbox, subject, i as i64],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO messages (id, account_id, thread_id, folder_id, date, sender_name, \
                 sender_email, recipients, subject, unread, starred, has_attachment) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'A', 'a@example.com', '', ?6, 0, 0, 0)",
                params![msg, account, thread, inbox, i as i64, subject],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO messages_fts \
                 (sender, recipients, subject, body, attachment_names, labels, message_id) \
                 VALUES ('A a@example.com', '', ?1, '', '', '', ?2)",
                params![subject, msg],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    #[test]
    fn search_over_ten_thousand_messages_stays_under_the_budget() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1", "me@example.com");
        seed_bulk_indexed(&core, "acc1", 10_000);

        let account = AccountId("acc1".into());
        let q = plan("unicorn");

        // Warm up once: the first query on a connection pays for plan
        // compilation, not representative of steady-state cost -- same
        // reasoning any microbenchmark excludes a cold first sample.
        let warmup = core.search(&account, &q, None, 20).unwrap();
        assert!(
            !warmup.threads.is_empty(),
            "the fixture seeds an 'unicorn' hit every 137th message"
        );

        // A single wall-clock sample is dominated by whatever else this
        // machine/CI runner is doing at that instant, in either
        // direction -- taking the minimum of several samples is the
        // standard fix: contention can only slow a run down, never make
        // it faster than the query's real cost, so the fastest of
        // several samples is the most honest lower bound and the least
        // likely to flip a fast query into a flaky failure.
        let mut best = std::time::Duration::from_secs(3600);
        for _ in 0..7 {
            let start = std::time::Instant::now();
            let results = core.search(&account, &q, None, 20).unwrap();
            let elapsed = start.elapsed();
            assert!(!results.threads.is_empty());
            best = best.min(elapsed);
        }
        assert!(
            best < std::time::Duration::from_millis(100),
            "search over 10k messages took {best:?} (best of 7 samples), budget is 100ms"
        );
    }
}
