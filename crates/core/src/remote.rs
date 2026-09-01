//! Remote folder discovery and the bridge from local ids to server ids.
//!
//! T-077: `feathermail_providers` walks IMAP `LIST` (RFC 6154 SPECIAL-USE)
//! and hands `Core` an already-parsed folder list — no IMAP wire type
//! (attributes, hierarchy delimiter, raw `LIST` response) crosses into
//! this crate (D9). [`Core::sync_folders`] reconciles that list against
//! the local `folders` table for one account.

use rusqlite::{params, OptionalExtension};

use crate::error::CoreError;
use crate::model::{AccountId, FolderKind};
use crate::store::{sql_err, Core};

/// One mailbox as returned by a live `LIST` walk, already resolved to a
/// [`FolderKind`] (T-077). `feathermail_providers` builds this from
/// `ImapSession::list_folders`'s `FolderListing`s; nothing IMAP-shaped
/// (SPECIAL-USE attribute strings, `\Noselect`) crosses into
/// `feathermail-core` itself (D9) — the provider crate is the one place
/// that knows what those mean and collapses them down to this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredFolder {
    /// The mailbox name exactly as the server's `LIST` returned it (e.g.
    /// `INBOX`, `[Gmail]/Sent Mail`, `Team/Ideas`). This — not `label` — is
    /// the folder's *identity*: two folders with the same display name
    /// (e.g. `Team/Ideas` and `Personal/Ideas`, both labeled "Ideas") are
    /// still two different mailboxes because their `remote_id`s differ.
    /// Stored verbatim in `folders.remote_id` so `RemoteLocator::remote_folder`
    /// (T-075) can hand it straight back to `SELECT`/`UID MOVE` without
    /// reconstructing it.
    pub remote_id: String,
    pub kind: FolderKind,
    /// Display name for the local `folders.name` column (and hence the
    /// sidebar). For system-kind folders this should be the canonical
    /// [`FolderKind::default_label`] (so a discovered `INBOX`/`\Archive`
    /// mailbox lines up with the placeholder row `Core::add_account`
    /// already seeds under that same label) rather than whatever the
    /// server happened to name the mailbox. Not unique by itself — see
    /// `remote_id` above — [`Core::sync_folders`] disambiguates it before
    /// writing if two discovered folders collide on it.
    pub label: String,
    /// `remote_id` of this folder's parent mailbox, if the server's
    /// hierarchy delimiter puts it under one (e.g. `Some("Team")` for
    /// `Team/Ideas`). `None` for a top-level mailbox or when the server
    /// reported no delimiter. Used only to fill `folders.parent_id`; it is
    /// still just an opaque identifier string here; the delimiter
    /// interpretation itself stays in `feathermail_providers` (D9).
    pub parent_remote_id: Option<String>,
    /// The server's hierarchy delimiter for this mailbox, verbatim, or
    /// `None` when the `LIST` response reported none. Stored in
    /// `folders.delimiter` and never interpreted here beyond splitting
    /// `remote_id` into path + leaf for a rename (T-060t). Core cannot
    /// recover it from `remote_id` on its own: a mailbox name may not
    /// contain its own delimiter, but nothing marks which character it is.
    pub delimiter: Option<char>,
}

impl Core {
    /// T-077: reconcile one account's `folders` table against a freshly
    /// walked server `LIST`.
    ///
    /// **Identity is `remote_id`, not the display name.** A folder already
    /// known under a given `remote_id` (from an earlier call to this
    /// method) is updated in place — `name` and `kind` refreshed, `id`
    /// untouched — which is also how a server-side rename is handled: same
    /// `remote_id`, new `name`, no second row. A folder with no matching
    /// `remote_id` yet is treated as new *unless* an existing row already
    /// has that exact display name and `remote_id IS NULL` — that is the
    /// one-time "adopt the placeholder" case (`Core::add_account`'s Inbox
    /// row, or a `Core::create_folder` row still waiting on its
    /// `OpKind::CreateFolder` to land) — in which case that row is
    /// upgraded to server-backed instead of a duplicate landing next to
    /// it. A row that already carries a *different* `remote_id` is never
    /// adopted by a name match; it is a different mailbox that happens to
    /// share a label, and picks up its own new row instead.
    ///
    /// `folders.id` is derived from `remote_id` (not the label) for the
    /// same reason: two different mailboxes must never compute the same
    /// id, or the second `INSERT` would collide with the first on the
    /// `PRIMARY KEY` and fail the whole batch instead of merging anything
    /// (`unique_folder_id` guards this explicitly rather than trusting the
    /// slug to always be collision-free).
    ///
    /// `folders.name` is no longer the identity column (T-079): the schema
    /// uniques `folders` on `(account_id, remote_id)`, and `name` only has
    /// to stay unique within one `parent_id` (`folders_account_parent_name`,
    /// `schema.sql`). So two discovered folders that share a display name
    /// (`Team/Ideas` and `Personal/Ideas`, both labeled "Ideas") land as two
    /// distinct rows under `folder.label` verbatim — no fallback to the raw
    /// `remote_id` needed, since `remote_id` alone already keeps them from
    /// colliding on identity or on the `PRIMARY KEY`.
    ///
    /// A folder that no longer appears in `discovered` (deleted or
    /// renamed on the server since the last walk) is left completely
    /// untouched: this method only INSERTs/UPDATEs rows named by
    /// `discovered`, it never DELETEs anything from `folders`. That is a
    /// deliberate choice, not an oversight — `folders.id` is the FK target
    /// for `threads.folder_id` and `messages.folder_id`, and those
    /// references are plain `REFERENCES`, not `ON DELETE CASCADE`, so a
    /// straight `DELETE` here would either fail the FK (if the folder
    /// still has mail) or, if it didn't, silently orphan the mail that
    /// hadn't been re-synced elsewhere yet — exactly the "vanished folder
    /// takes the mail with it" failure T-077 was asked to avoid. Once a
    /// folder truly disappears remotely, further sync of that mailbox
    /// simply stops (nothing walks it anymore); its row and any mail
    /// already local to it just stay put until something else (account
    /// removal, a future "hide empty stale folders" UI feature) decides
    /// what to do with it. That follow-up is out of scope here.
    ///
    /// `parent_id` is filled from `parent_remote_id` in a second pass over
    /// `discovered`, after every row in the batch has already been
    /// written — so it does not matter whether a child's entry comes
    /// before or after its parent's in `discovered` (IMAP's `LIST` makes
    /// no such ordering promise). A parent whose own `remote_id` never
    /// shows up in `discovered` (e.g. filtered out as `\Noselect` by
    /// `feathermail_providers`) simply leaves the child's `parent_id`
    /// unset, i.e. top-level, rather than erroring.
    ///
    /// One transaction for the whole batch (D13), same reasoning
    /// `sync_store::upsert_headers` documents for its batch: `folders` is
    /// small, so the win is not commit throughput, it is the error path —
    /// without a transaction, a `LIST` walk that errors out after writing
    /// half its rows would leave the table at a self-inconsistent halfway
    /// point instead of either the old state or the new one.
    pub fn sync_folders(
        &mut self,
        account_id: &AccountId,
        discovered: &[DiscoveredFolder],
    ) -> Result<(), CoreError> {
        self.require_account(account_id.as_str())?;
        let account = account_id.as_str();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        for folder in discovered {
            sync_one_folder(&tx, account, folder)?;
        }
        for folder in discovered {
            link_parent(&tx, account, folder)?;
        }
        tx.commit().map_err(sql_err)?;
        Ok(())
    }
}

/// Upserts one [`DiscoveredFolder`] by `remote_id` identity (see
/// [`Core::sync_folders`]'s doc comment for the full decision tree).
fn sync_one_folder(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    folder: &DiscoveredFolder,
) -> Result<(), CoreError> {
    // 1. Already known under this exact remote_id: rename/kind update in
    //    place, id untouched.
    let by_remote_id: Option<String> = tx
        .query_row(
            "SELECT id FROM folders WHERE account_id = ?1 AND remote_id = ?2",
            params![account, folder.remote_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if let Some(id) = by_remote_id {
        tx.execute(
            "UPDATE folders SET name = ?1, kind = ?2, delimiter = ?3 WHERE id = ?4",
            params![
                folder.label,
                folder.kind.as_str(),
                folder.delimiter.map(|c| c.to_string()),
                id
            ],
        )
        .map_err(sql_err)?;
        // T-060u: the server still lists this mailbox, so it is not deleted
        // -- unless the `DELETE` simply has not run yet. The pending
        // operation is the difference between "the delete failed, put the
        // folder back" and "the user just asked and we are about to do it";
        // without this guard a `LIST` between the two would undo the user's
        // action in front of them.
        tx.execute(
            "UPDATE folders SET deleted_at = NULL
             WHERE id = ?1 AND deleted_at IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM operations o
                   WHERE o.account_id = ?2 AND o.target_id = ?1
                     AND o.op = 'delete_folder'
                     AND o.status IN ('local', 'pending', 'running', 'blocked')
               )",
            params![id, account],
        )
        .map_err(sql_err)?;
        return Ok(());
    }

    // 2. A remote_id-less placeholder sitting under this exact label:
    //    adopt it (Core::add_account's Inbox row, or a Core::create_folder
    //    row whose CreateFolder op hasn't landed yet, turns out to already
    //    exist on the server).
    //    A tombstone (T-060u) qualifies too: its `DELETE` is done, its
    //    identity is gone, and a mailbox with that name existing again means
    //    the folder exists again -- so adopting it is more truthful than
    //    opening a second row beside a hidden one that has the same name.
    let placeholder: Option<String> = tx
        .query_row(
            "SELECT id FROM folders WHERE account_id = ?1 AND name = ?2 AND remote_id IS NULL",
            params![account, folder.label],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if let Some(id) = placeholder {
        tx.execute(
            "UPDATE folders SET remote_id = ?1, kind = ?2, delimiter = ?3, deleted_at = NULL
             WHERE id = ?4",
            params![
                folder.remote_id,
                folder.kind.as_str(),
                folder.delimiter.map(|c| c.to_string()),
                id
            ],
        )
        .map_err(sql_err)?;
        // Before this row had a remote id, the worker could only record a
        // local "cannot resolve this placeholder" attempt. That backoff
        // must not survive discovery: it would delay the first real SELECT
        // by up to fifteen minutes even though the folder just became
        // reachable. No valid cursor can exist for a remote-id-less row,
        // so dropping its sync state is both safe and the only honest
        // starting point for the first server-backed pass.
        tx.execute(
            "DELETE FROM sync_state WHERE account_id = ?1 AND folder_id = ?2",
            params![account, id],
        )
        .map_err(sql_err)?;
        return Ok(());
    }

    // 3. Genuinely new folder. `folder.label` is written as-is (T-079: two
    //    discovered folders sharing a label is fine now, since `remote_id`
    //    -- not `name` -- is what `folders` uniques on) and `unique_folder_id`
    //    guards the id the same way `remote_id` already guards identity.
    let id = unique_folder_id(tx, account, &folder.remote_id)?;
    tx.execute(
        "INSERT INTO folders (id, account_id, remote_id, name, kind, delimiter)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            account,
            folder.remote_id,
            folder.label,
            folder.kind.as_str(),
            folder.delimiter.map(|c| c.to_string())
        ],
    )
    .map_err(sql_err)?;
    Ok(())
}

/// `{account}:{slug(remote_id)}`, walked forward with a numeric suffix
/// until free. `remote_id` is unique per account (IMAP guarantees mailbox
/// names are unique), but its slug is a lossy ASCII collapse of it and is
/// not guaranteed injective (e.g. `Team/Ideas` and `Team-Ideas` both slug
/// to `team-ideas`), so this checks rather than assumes: a naive
/// `id = format!("{account}:{slug}")` would otherwise let two different
/// mailboxes compute the same `PRIMARY KEY` and fail the second `INSERT`
/// (and, with it, the whole batch — sync_folders_two_folders_whose_slugs_collide_still_get_two_rows
/// in this module's tests exercises exactly that instead of trusting it
/// by inspection).
fn unique_folder_id(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    remote_id: &str,
) -> Result<String, CoreError> {
    let base = format!("{account}:{}", remote_id_slug(remote_id));
    let mut candidate = base.clone();
    let mut suffix = 2;
    loop {
        let taken: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM folders WHERE id = ?1",
                params![candidate],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if taken.is_none() {
            return Ok(candidate);
        }
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
}

/// Lossy ASCII-lowercase collapse of a remote mailbox path/name into a
/// PRIMARY KEY-friendly slug: keep alphanumerics, collapse every run of
/// anything else to a single `-`, trim leading/trailing `-`. Not assumed
/// injective — see [`unique_folder_id`].
fn remote_id_slug(remote_id: &str) -> String {
    let mut out = String::with_capacity(remote_id.len());
    let mut pending_dash = false;
    for c in remote_id.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            pending_dash = false;
        } else if !out.is_empty() && !pending_dash {
            out.push('-');
            pending_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "folder".to_string()
    } else {
        out
    }
}

/// Second-pass fill of `folders.parent_id` from [`DiscoveredFolder::parent_remote_id`]
/// (see [`Core::sync_folders`]'s doc comment for why this runs after every
/// row in the batch is already written).
fn link_parent(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    folder: &DiscoveredFolder,
) -> Result<(), CoreError> {
    let Some(parent_remote_id) = &folder.parent_remote_id else {
        return Ok(());
    };
    let parent_id: Option<String> = tx
        .query_row(
            "SELECT id FROM folders WHERE account_id = ?1 AND remote_id = ?2",
            params![account, parent_remote_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    let Some(parent_id) = parent_id else {
        return Ok(());
    };
    tx.execute(
        "UPDATE folders SET parent_id = ?1 WHERE account_id = ?2 AND remote_id = ?3",
        params![parent_id, account, folder.remote_id],
    )
    .map_err(sql_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::model::FolderId;
    use crate::provider::{ApplyError, MailProvider};
    use rusqlite::params as rparams;
    use std::collections::HashSet;

    fn account(core: &Core, id: &str) {
        let conn = core.db.conn();
        conn.execute(
            "INSERT INTO accounts (
                id, name, email, provider, status, download_policy, created_at, updated_at
            ) VALUES (?1, 'Name', ?2, 'generic', 'synced', 'recent', 0, 0)",
            rparams![id, format!("{id}@example.com")],
        )
        .unwrap();
    }

    struct Row {
        id: String,
        remote_id: Option<String>,
        name: String,
        kind: String,
        parent_id: Option<String>,
    }

    fn folder_by_remote_id(core: &Core, account_id: &str, remote_id: &str) -> Row {
        core.db
            .conn()
            .query_row(
                "SELECT id, remote_id, name, kind, parent_id FROM folders \
                 WHERE account_id = ?1 AND remote_id = ?2",
                rparams![account_id, remote_id],
                |row| {
                    Ok(Row {
                        id: row.get(0)?,
                        remote_id: row.get(1)?,
                        name: row.get(2)?,
                        kind: row.get(3)?,
                        parent_id: row.get(4)?,
                    })
                },
            )
            .unwrap()
    }

    fn folder_row(core: &Core, id: &str) -> Row {
        core.db
            .conn()
            .query_row(
                "SELECT id, remote_id, name, kind, parent_id FROM folders WHERE id = ?1",
                rparams![id],
                |row| {
                    Ok(Row {
                        id: row.get(0)?,
                        remote_id: row.get(1)?,
                        name: row.get(2)?,
                        kind: row.get(3)?,
                        parent_id: row.get(4)?,
                    })
                },
            )
            .unwrap()
    }

    fn folder_count(core: &Core, account_id: &str) -> i64 {
        core.db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM folders WHERE account_id = ?1",
                rparams![account_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn inbox() -> DiscoveredFolder {
        DiscoveredFolder {
            remote_id: "INBOX".into(),
            kind: FolderKind::Inbox,
            label: "Inbox".into(),
            parent_remote_id: None,
            delimiter: Some('/'),
        }
    }

    fn archive() -> DiscoveredFolder {
        DiscoveredFolder {
            remote_id: "Archive".into(),
            kind: FolderKind::Archive,
            label: "Archive".into(),
            parent_remote_id: None,
            delimiter: Some('/'),
        }
    }

    fn notes() -> DiscoveredFolder {
        DiscoveredFolder {
            remote_id: "Notes".into(),
            kind: FolderKind::Custom,
            label: "Notes".into(),
            parent_remote_id: None,
            delimiter: Some('/'),
        }
    }

    #[test]
    fn sync_folders_unknown_account_is_account_not_found() {
        let mut core = Core::memory().unwrap();
        let err = core
            .sync_folders(&AccountId("nope".into()), &[inbox()])
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::AccountNotFound);
    }

    #[test]
    fn sync_folders_writes_remote_id_and_kind() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        core.sync_folders(&AccountId("acc".into()), &[inbox(), archive(), notes()])
            .unwrap();

        let row = folder_by_remote_id(&core, "acc", "INBOX");
        assert_eq!(row.name, "Inbox");
        assert_eq!(row.kind, "inbox");

        let row = folder_by_remote_id(&core, "acc", "Archive");
        assert_eq!(row.name, "Archive");
        assert_eq!(row.kind, "archive");

        let row = folder_by_remote_id(&core, "acc", "Notes");
        assert_eq!(row.name, "Notes");
        assert_eq!(row.kind, "custom");
    }

    /// D29: two identical walks in a row must not duplicate rows nor fail
    /// on the `UNIQUE (account_id, remote_id)`/`PRIMARY KEY` constraints.
    #[test]
    fn sync_folders_is_idempotent_across_two_runs() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        let discovered = vec![inbox(), archive(), notes()];
        core.sync_folders(&AccountId("acc".into()), &discovered)
            .unwrap();
        let first_count = folder_count(&core, "acc");

        core.sync_folders(&AccountId("acc".into()), &discovered)
            .unwrap();
        let second_count = folder_count(&core, "acc");

        assert_eq!(first_count, second_count);
        assert_eq!(second_count, 3);
    }

    /// A folder already seeded by `Core::add_account` under the canonical
    /// label (e.g. the `Inbox` row created with `remote_id = NULL`) is
    /// adopted in place, not duplicated next to a second `Inbox` row.
    #[test]
    fn sync_folders_adopts_the_placeholder_inbox_row_created_at_add_account() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, name, kind) VALUES ('acc:inbox', 'acc', 'Inbox', 'inbox')",
                [],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO sync_state (account_id, folder_id, last_attempt_at, consecutive_failures) VALUES ('acc', 'acc:inbox', 100, 8)",
                [],
            )
            .unwrap();

        core.sync_folders(&AccountId("acc".into()), &[inbox()])
            .unwrap();

        assert_eq!(folder_count(&core, "acc"), 1);
        let row = folder_row(&core, "acc:inbox");
        assert_eq!(row.remote_id.as_deref(), Some("INBOX"));
        let attempts: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sync_state WHERE account_id = 'acc' AND folder_id = 'acc:inbox'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            attempts, 0,
            "placeholder backoff must not delay the first real sync"
        );
    }

    /// A folder that disappears from the next `LIST` walk keeps its row
    /// (and, transitively, any mail filed under it) — `sync_folders` never
    /// deletes.
    #[test]
    fn sync_folders_leaves_a_vanished_folder_and_its_mail_alone() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        core.sync_folders(&AccountId("acc".into()), &[inbox(), notes()])
            .unwrap();
        let notes_id = folder_by_remote_id(&core, "acc", "Notes").id;
        core.db
            .conn()
            .execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread, starred)
                 VALUES ('t1', 'acc', ?1, 'Hi', 'Hi', 0, 0, 0)",
                rparams![notes_id],
            )
            .unwrap();

        // Notes is gone from the next walk.
        core.sync_folders(&AccountId("acc".into()), &[inbox()])
            .unwrap();

        assert_eq!(folder_count(&core, "acc"), 2, "the Notes row is kept");
        let row = folder_by_remote_id(&core, "acc", "Notes");
        assert_eq!(row.name, "Notes");
        let thread_folder: String = core
            .db
            .conn()
            .query_row("SELECT folder_id FROM threads WHERE id = 't1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(thread_folder, notes_id, "the thread's mail is untouched");
    }

    /// A custom folder created locally through `Core::create_folder` (no
    /// server round trip yet, `remote_id IS NULL`) is not disturbed by a
    /// walk that never mentions it.
    #[test]
    fn sync_folders_does_not_touch_an_unrelated_local_only_custom_folder() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        let local_id = core
            .create_folder(&AccountId("acc".into()), "Ideas")
            .unwrap();
        assert_eq!(local_id, FolderId("acc:ideas".into()));

        core.sync_folders(&AccountId("acc".into()), &[inbox()])
            .unwrap();

        let row = folder_row(&core, "acc:ideas");
        assert_eq!(row.remote_id, None, "still local-only, never touched");
        assert_eq!(row.name, "Ideas");
        assert_eq!(row.kind, "custom");
    }

    /// A provider that refuses every operation it's handed, whatever kind
    /// it is -- this module's own copy of `queue::tests::AlwaysFailProvider`
    /// (that one is private to its module).
    struct AlwaysFail(ApplyError);

    impl MailProvider for AlwaysFail {
        fn apply(&mut self, _op: &crate::model::Operation) -> Result<(), ApplyError> {
            Err(self.0)
        }
    }

    /// T-084 x T-077: a folder whose `CreateFolder` failed terminally is,
    /// underneath, still just a `remote_id IS NULL` placeholder row --
    /// exactly the shape `sync_one_folder`'s placeholder-adoption case
    /// (case 2 above) looks for. If a later `sync_folders` walk discovers a
    /// server mailbox under that same name (this account creating it
    /// through another client after our attempt failed here, say), the row
    /// is adopted like any other unconfirmed placeholder -- and
    /// `Folder::create_failed` must clear the instant that happens, even
    /// though the original `create_folder` operation row is still sitting
    /// in `operations` as `failed` forever (nothing ever goes back and
    /// touches it). That is exactly why `FOLDER_SUMMARY_SQL` gates the flag
    /// on `remote_id IS NULL`, not on "a failed op exists" alone.
    #[test]
    fn adopting_a_create_failed_placeholder_clears_the_flag() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        let id = core
            .create_folder(&AccountId("acc".into()), "Ideas")
            .unwrap();
        let mut failing = AlwaysFail(ApplyError::Unsupported);
        core.tick(&mut failing).unwrap();

        let before = core.list_folders(&AccountId("acc".into())).unwrap();
        assert!(
            before
                .iter()
                .find(|f| f.folder.id == id)
                .unwrap()
                .folder
                .create_failed,
            "sanity: the failure must be visible before the adoption this test is about"
        );

        let discovered = DiscoveredFolder {
            remote_id: "Ideas".into(),
            kind: FolderKind::Custom,
            label: "Ideas".into(),
            parent_remote_id: None,
            delimiter: Some('/'),
        };
        core.sync_folders(&AccountId("acc".into()), &[discovered])
            .unwrap();

        let row = folder_row(&core, id.as_str());
        assert_eq!(
            row.remote_id.as_deref(),
            Some("Ideas"),
            "the placeholder must have been adopted onto the same row"
        );
        let after = core.list_folders(&AccountId("acc".into())).unwrap();
        assert!(
            !after
                .iter()
                .find(|f| f.folder.id == id)
                .unwrap()
                .folder
                .create_failed,
            "once the server confirms the folder exists, the ghost flag must clear -- \
             the failed operation row underneath is irrelevant once remote_id is set"
        );
    }

    // --- T-077 review: remote_id is identity, not the display name --------

    fn team_ideas() -> DiscoveredFolder {
        DiscoveredFolder {
            remote_id: "Team/Ideas".into(),
            kind: FolderKind::Custom,
            label: "Ideas".into(),
            parent_remote_id: Some("Team".into()),
            delimiter: Some('/'),
        }
    }

    fn personal_ideas() -> DiscoveredFolder {
        DiscoveredFolder {
            remote_id: "Personal/Ideas".into(),
            kind: FolderKind::Custom,
            label: "Ideas".into(),
            parent_remote_id: Some("Personal".into()),
            delimiter: Some('/'),
        }
    }

    fn archive_ideas() -> DiscoveredFolder {
        DiscoveredFolder {
            remote_id: "Archive/Ideas".into(),
            kind: FolderKind::Custom,
            label: "Ideas".into(),
            parent_remote_id: Some("Archive".into()),
            delimiter: Some('/'),
        }
    }

    /// The bug the review caught: two different mailboxes that both
    /// display as "Ideas" must not collapse into one local row, and
    /// neither `remote_id` may be silently dropped. T-079 fixed the actual
    /// bug this test was working around: both rows now keep the plain
    /// "Ideas" label (identity lives in `remote_id`/`PRIMARY KEY`, not in
    /// `name`, so there is no need for either one to fall back to its raw
    /// server path just to stay distinguishable in the schema).
    #[test]
    fn two_folders_with_the_same_leaf_label_stay_as_two_rows() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        core.sync_folders(&AccountId("acc".into()), &[team_ideas(), personal_ideas()])
            .unwrap();

        assert_eq!(folder_count(&core, "acc"), 2);
        let team = folder_by_remote_id(&core, "acc", "Team/Ideas");
        let personal = folder_by_remote_id(&core, "acc", "Personal/Ideas");
        assert_ne!(team.id, personal.id, "distinct rows");
        assert_ne!(team.remote_id, personal.remote_id, "distinct identity");
        assert_eq!(team.name, "Ideas", "human-readable, not a raw server path");
        assert_eq!(
            personal.name, "Ideas",
            "human-readable, not a raw server path"
        );
    }

    /// Same two folders survive a second identical walk unchanged — no
    /// growth, no id/name churn, no constraint failure.
    #[test]
    fn two_folders_with_the_same_leaf_label_are_idempotent_across_two_runs() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        let discovered = vec![team_ideas(), personal_ideas()];
        core.sync_folders(&AccountId("acc".into()), &discovered)
            .unwrap();
        let team_before = folder_by_remote_id(&core, "acc", "Team/Ideas");
        let personal_before = folder_by_remote_id(&core, "acc", "Personal/Ideas");

        core.sync_folders(&AccountId("acc".into()), &discovered)
            .unwrap();

        assert_eq!(folder_count(&core, "acc"), 2);
        let team_after = folder_by_remote_id(&core, "acc", "Team/Ideas");
        let personal_after = folder_by_remote_id(&core, "acc", "Personal/Ideas");
        assert_eq!(team_before.id, team_after.id);
        assert_eq!(team_before.name, team_after.name);
        assert_eq!(personal_before.id, personal_after.id);
        assert_eq!(personal_before.name, personal_after.name);
    }

    /// A rename on the server (same `remote_id`, new label) updates the
    /// existing row in place; it must not create a second one.
    #[test]
    fn renaming_a_folder_on_the_server_updates_name_not_a_new_row() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        core.sync_folders(&AccountId("acc".into()), &[notes()])
            .unwrap();
        let before = folder_by_remote_id(&core, "acc", "Notes");

        let renamed = DiscoveredFolder {
            remote_id: "Notes".into(),
            kind: FolderKind::Custom,
            label: "Scratchpad".into(),
            parent_remote_id: None,
            delimiter: Some('/'),
        };
        core.sync_folders(&AccountId("acc".into()), &[renamed])
            .unwrap();

        assert_eq!(folder_count(&core, "acc"), 1, "still one row");
        let after = folder_by_remote_id(&core, "acc", "Notes");
        assert_eq!(after.id, before.id, "same row, not a new one");
        assert_eq!(after.name, "Scratchpad");
    }

    /// A row that already carries a *different* `remote_id` must never be
    /// adopted just because a newly discovered folder shares its label —
    /// only a `remote_id IS NULL` placeholder may be adopted.
    #[test]
    fn a_row_with_a_different_remote_id_already_set_is_not_adopted_by_name() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        core.sync_folders(
            &AccountId("acc".into()),
            &[DiscoveredFolder {
                remote_id: "OtherRemote".into(),
                kind: FolderKind::Custom,
                label: "Ideas".into(),
                parent_remote_id: None,
                delimiter: Some('/'),
            }],
        )
        .unwrap();

        core.sync_folders(
            &AccountId("acc".into()),
            &[DiscoveredFolder {
                remote_id: "NewRemote".into(),
                kind: FolderKind::Custom,
                label: "Ideas".into(),
                parent_remote_id: None,
                delimiter: Some('/'),
            }],
        )
        .unwrap();

        assert_eq!(folder_count(&core, "acc"), 2, "two distinct mailboxes");
        let other = folder_by_remote_id(&core, "acc", "OtherRemote");
        let new = folder_by_remote_id(&core, "acc", "NewRemote");
        assert_ne!(other.id, new.id);
        assert_ne!(new.remote_id, other.remote_id);
    }

    /// Defensive: even if two different `remote_id`s happen to slug down
    /// to the same ASCII id base, the second folder still gets its own
    /// row instead of failing the batch on a `PRIMARY KEY` collision.
    /// Verified by running the code, not by inspecting `remote_id_slug`.
    #[test]
    fn two_folders_whose_ids_would_collide_still_get_two_rows() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        let discovered = vec![
            DiscoveredFolder {
                remote_id: "Team/Ideas".into(),
                kind: FolderKind::Custom,
                label: "Team Ideas".into(),
                parent_remote_id: None,
                delimiter: Some('/'),
            },
            DiscoveredFolder {
                remote_id: "Team-Ideas".into(),
                kind: FolderKind::Custom,
                label: "Team-Ideas Folder".into(),
                parent_remote_id: None,
                delimiter: Some('/'),
            },
        ];

        core.sync_folders(&AccountId("acc".into()), &discovered)
            .unwrap();

        assert_eq!(folder_count(&core, "acc"), 2, "no row was dropped/merged");
        let first = folder_by_remote_id(&core, "acc", "Team/Ideas");
        let second = folder_by_remote_id(&core, "acc", "Team-Ideas");
        assert_ne!(first.id, second.id);

        // Idempotent: running it again doesn't grow or fail either.
        core.sync_folders(&AccountId("acc".into()), &discovered)
            .unwrap();
        assert_eq!(folder_count(&core, "acc"), 2);
    }

    // --- T-077 review: parent_id from parent_remote_id ---------------------

    #[test]
    fn parent_id_is_filled_from_parent_remote_id() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        let team = DiscoveredFolder {
            remote_id: "Team".into(),
            kind: FolderKind::Custom,
            label: "Team".into(),
            parent_remote_id: None,
            delimiter: Some('/'),
        };
        core.sync_folders(&AccountId("acc".into()), &[team, team_ideas()])
            .unwrap();

        let team_row = folder_by_remote_id(&core, "acc", "Team");
        let ideas_row = folder_by_remote_id(&core, "acc", "Team/Ideas");
        assert_eq!(ideas_row.parent_id.as_deref(), Some(team_row.id.as_str()));
    }

    /// Ordering in `discovered` must not matter: the child can appear
    /// before its parent and still end up linked, since `parent_id` is
    /// filled in a second pass over the whole batch.
    #[test]
    fn parent_id_is_filled_even_when_the_child_comes_first_in_the_list() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        let team = DiscoveredFolder {
            remote_id: "Team".into(),
            kind: FolderKind::Custom,
            label: "Team".into(),
            parent_remote_id: None,
            delimiter: Some('/'),
        };
        // Child first, parent second.
        core.sync_folders(&AccountId("acc".into()), &[team_ideas(), team])
            .unwrap();

        let team_row = folder_by_remote_id(&core, "acc", "Team");
        let ideas_row = folder_by_remote_id(&core, "acc", "Team/Ideas");
        assert_eq!(ideas_row.parent_id.as_deref(), Some(team_row.id.as_str()));
    }

    /// A parent that never shows up in `discovered` (e.g. a `\Noselect`
    /// container `feathermail_providers` filtered out) leaves `parent_id`
    /// unset rather than erroring.
    #[test]
    fn missing_parent_leaves_parent_id_unset() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        core.sync_folders(&AccountId("acc".into()), &[team_ideas()])
            .unwrap();

        let ideas_row = folder_by_remote_id(&core, "acc", "Team/Ideas");
        assert_eq!(ideas_row.parent_id, None);
    }

    // --- T-079 acceptance: three "Ideas" in three branches ----------------

    /// The plan's own acceptance line for T-079: three real folders in
    /// three different branches, all leaf-named "Ideas", must each show up
    /// human-readably (not one of them falling back to its raw server
    /// path) and survive a repeat walk without duplicating or rewriting
    /// identity. This is exactly what `UNIQUE (account_id, name)` used to
    /// forbid -- the second and third of these three would previously have
    /// failed the whole batch (or, after T-077's workaround, landed with
    /// their raw `remote_id` as `name` instead of "Ideas") -- and what the
    /// T-079 schema/`remote.rs` changes exist to fix.
    #[test]
    fn three_ideas_in_three_branches_are_all_human_readable_and_survive_a_repeat_walk() {
        let mut core = Core::memory().unwrap();
        account(&core, "acc");
        let discovered = vec![team_ideas(), personal_ideas(), archive_ideas()];

        core.sync_folders(&AccountId("acc".into()), &discovered)
            .unwrap();

        assert_eq!(folder_count(&core, "acc"), 3, "three distinct rows");
        let team = folder_by_remote_id(&core, "acc", "Team/Ideas");
        let personal = folder_by_remote_id(&core, "acc", "Personal/Ideas");
        let archive = folder_by_remote_id(&core, "acc", "Archive/Ideas");
        // Each row keeps its own remote_id -- the folder's real identity.
        assert_eq!(team.remote_id.as_deref(), Some("Team/Ideas"));
        assert_eq!(personal.remote_id.as_deref(), Some("Personal/Ideas"));
        assert_eq!(archive.remote_id.as_deref(), Some("Archive/Ideas"));
        let ids: HashSet<_> = [&team.id, &personal.id, &archive.id].into_iter().collect();
        assert_eq!(ids.len(), 3, "three distinct ids");
        // All three are human-readable "Ideas", not a raw server path.
        assert_eq!(team.name, "Ideas");
        assert_eq!(personal.name, "Ideas");
        assert_eq!(archive.name, "Ideas");

        // A second, identical walk must not duplicate rows or rewrite
        // remote_id/id -- D29.
        core.sync_folders(&AccountId("acc".into()), &discovered)
            .unwrap();

        assert_eq!(folder_count(&core, "acc"), 3, "no duplicates on repeat");
        let team_again = folder_by_remote_id(&core, "acc", "Team/Ideas");
        let personal_again = folder_by_remote_id(&core, "acc", "Personal/Ideas");
        let archive_again = folder_by_remote_id(&core, "acc", "Archive/Ideas");
        assert_eq!(team_again.id, team.id);
        assert_eq!(team_again.remote_id, team.remote_id);
        assert_eq!(team_again.name, "Ideas");
        assert_eq!(personal_again.id, personal.id);
        assert_eq!(personal_again.remote_id, personal.remote_id);
        assert_eq!(personal_again.name, "Ideas");
        assert_eq!(archive_again.id, archive.id);
        assert_eq!(archive_again.remote_id, archive.remote_id);
        assert_eq!(archive_again.name, "Ideas");
    }
}
