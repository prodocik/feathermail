//! Resolving a local thread to its IMAP coordinates (T-075).
//!
//! [`Core`] implements [`RemoteLocator`] over its own SQLite handle
//! (`Core.db` is `pub(crate)`, reachable from here since this module lives
//! inside `feathermail-core`). `feathermail_providers::ImapMailProvider`
//! (T-025) is the only caller: it asks `thread_messages` for the mailbox +
//! UID pairs a queued operation's thread currently maps to, and
//! `remote_folder` for the IMAP mailbox path a `Move`/`Archive`/`Trash`
//! operation should land in.
//!
//! System destinations (`ARCHIVE_FOLDER_KEY`/`TRASH_FOLDER_KEY`) are
//! resolved by `folders.kind`, never by a guessed name (T-076/T-077's
//! decision that the folder, not the local overlay, is the source of
//! truth for what a real mailbox is called). A folder of the right kind
//! that has never been matched against a server `LIST` (`remote_id IS
//! NULL`, T-077) — or the account has no folder of that kind at all — is
//! `ApplyError::Unsupported`: an honest refusal, not an invented mailbox
//! name that would send an operation somewhere wrong.

use rusqlite::{params, OptionalExtension};

use crate::model::{AccountId, FolderKind, OpKind};
use crate::provider::{
    ApplyError, RemoteLocator, RemoteMessage, ARCHIVE_FOLDER_KEY, TRASH_FOLDER_KEY,
};
use crate::store::Core;

impl RemoteLocator for Core {
    /// Every `(folder, uid)` pair `thread_id` currently has rows for,
    /// across however many folders it straddles. A message whose folder
    /// has no `remote_id` yet, or that has no `provider_uid` yet (not
    /// synced from the server), is silently left out — same "no honest
    /// mailbox name/UID to act on" rule `Core::open_body` (T-024) applies —
    /// rather than erroring the whole thread out over one unresolved row.
    /// An empty result (thread not found, or none of its messages resolve)
    /// is `Ok(vec![])`; `ImapMailProvider` treats an empty list as
    /// `ApplyError::NotFound` itself, so this does not have to guess which
    /// of those two cases it is looking at.
    fn thread_messages(
        &self,
        account_id: &AccountId,
        thread_id: &str,
    ) -> Result<Vec<RemoteMessage>, ApplyError> {
        let conn = self.db.conn();
        // Once a destination sync has optimistically rehomed a message, its
        // current row no longer contains the source UID the queued MOVE must
        // apply.  Active intents are the authoritative source locator until
        // ACK; exclude their current rows and add the captured source pair
        // below.  ACKed intents are intentionally omitted: a later command
        // against the same thread must target the destination, not replay an
        // already-confirmed source MOVE.
        let mut stmt = conn
            .prepare(
                "SELECT f.remote_id, m.provider_uid \
                 FROM messages m JOIN folders f ON f.id = m.folder_id \
                 WHERE m.account_id = ?1 AND m.thread_id = ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM operation_moves om
                       JOIN operations o ON o.id = om.operation_id
                       WHERE om.message_id = m.id AND o.account_id = m.account_id
                         AND o.status IN ('pending', 'running')
                   )
                 UNION ALL
                 SELECT om.source_remote_id, om.source_uid
                 FROM operation_moves om
                 JOIN operations o ON o.id = om.operation_id
                 WHERE o.account_id = ?1 AND o.target_id = ?2
                   AND o.status IN ('pending', 'running')",
            )
            .map_err(sql_err_apply)?;
        let rows = stmt
            .query_map(params![account_id.as_str(), thread_id], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                ))
            })
            .map_err(sql_err_apply)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err_apply)?;

        let messages = rows
            .into_iter()
            .filter_map(|(remote_id, provider_uid)| {
                let folder = remote_id.filter(|s| !s.is_empty())?;
                let uid = provider_uid.and_then(|v| u32::try_from(v).ok())?;
                Some(RemoteMessage { folder, uid })
            })
            .collect();
        Ok(messages)
    }

    fn thread_messages_for_operation(
        &self,
        account_id: &AccountId,
        thread_id: &str,
        operation_id: &str,
    ) -> Result<Vec<RemoteMessage>, ApplyError> {
        let conn = self.db.conn();
        let mut stmt = conn
            .prepare(
                "SELECT om.source_remote_id, om.source_uid
                 FROM operation_moves om
                 JOIN operations o ON o.id = om.operation_id
                 JOIN messages m ON m.id = om.message_id
                 WHERE om.operation_id = ?1 AND o.account_id = ?2
                   AND o.target_id = ?3
                   AND o.status IN ('pending', 'running')
                 ORDER BY om.message_id",
            )
            .map_err(sql_err_apply)?;
        let rows = stmt
            .query_map(
                params![operation_id, account_id.as_str(), thread_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(sql_err_apply)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err_apply)?;
        if !rows.is_empty() {
            return Ok(rows
                .into_iter()
                .filter_map(|(folder, uid)| {
                    let folder = (!folder.is_empty()).then_some(folder)?;
                    Some(RemoteMessage {
                        folder,
                        uid: u32::try_from(uid).ok()?,
                    })
                })
                .collect());
        }

        // A pre-T-076 Move has no per-message intent. Keep its original
        // locator behavior: the optimistic thread folder may already point
        // at the destination while the physical message row still carries
        // the source mailbox/UID. Archive and Trash are different. Core can
        // queue those as local overlays when no real special-use folder was
        // known at dispatch time; falling back here would make a later LIST
        // discovery silently turn that overlay into a remote MOVE.
        //
        // Query the operation kind rather than inferring it from the current
        // thread flags: flags are user-visible state and can be changed by a
        // subsequent command before the worker claims this operation. The
        // status guard also keeps this helper from replaying an already ACKed
        // operation if a caller invokes it outside the queue.
        let operation: Option<(String, String, String)> = conn
            .query_row(
                "SELECT op, status, payload FROM operations
                 WHERE id = ?1 AND account_id = ?2 AND target_id = ?3",
                params![operation_id, account_id.as_str(), thread_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sql_err_apply)?;
        let live = operation
            .as_ref()
            .filter(|(_, status, _)| matches!(status.as_str(), "pending" | "running"));
        if live.is_some_and(|(kind, _, _)| kind == OpKind::Move.as_str()) {
            return self.thread_messages(account_id, thread_id);
        }
        // `PermanentDelete` is the one operation whose targets must never be
        // re-resolved live: `UID EXPUNGE` cannot be undone, so a reply that
        // joined the thread while the operation sat in the queue (offline, or
        // in network backoff) would be destroyed by a command issued before it
        // arrived. It cannot use `operation_moves` either -- the queue reads a
        // row there as an active MOVE and would rehome the message -- so
        // `store::permanent_delete_payload` freezes the coordinates in the
        // operation's own payload at dispatch, and this reads them back.
        // An operation queued before that payload existed resolves to nothing
        // and fails closed as `NotFound`, which is the right way round for an
        // irreversible delete.
        if let Some((_, _, payload)) =
            live.filter(|(kind, _, _)| kind == OpKind::PermanentDelete.as_str())
        {
            return Ok(permanent_delete_targets(payload));
        }

        // Core-created Archive/Trash overlays deliberately have no durable
        // move intent and fail closed, as do active intents whose source row
        // was not usable. Never guess from the optimistically rehomed thread.
        Ok(Vec::new())
    }

    /// `folder_key` is either [`ARCHIVE_FOLDER_KEY`]/[`TRASH_FOLDER_KEY`]
    /// (resolved by `folders.kind`) or a real local `folders.id` (as
    /// carried in a `Move` operation's payload, resolved by that id
    /// directly). Either path ends the same way: a `remote_id` that is
    /// `NULL` or empty is `ApplyError::Unsupported`, same as a missing row.
    fn remote_folder(
        &self,
        account_id: &AccountId,
        folder_key: &str,
    ) -> Result<String, ApplyError> {
        let conn = self.db.conn();
        let remote_id: Option<Option<String>> = if let Some(kind) = system_kind(folder_key) {
            // Only rows that actually resolve, and a deterministic pick
            // among them. An account can hold more than one folder of a
            // system kind -- a placeholder T-077 could not adopt by name
            // (a server whose Archive mailbox is called something else
            // entirely) sits alongside the real discovered one -- and an
            // unqualified `LIMIT 1` would be free to return the
            // unresolved placeholder, refusing every Archive operation on
            // that account while a perfectly good mailbox name sat in the
            // next row. Ordering by `id` also keeps repeated calls
            // answering the same way, which `LIMIT 1` alone does not
            // promise.
            conn.query_row(
                "SELECT remote_id FROM folders \
                 WHERE account_id = ?1 AND kind = ?2 \
                   AND remote_id IS NOT NULL AND remote_id <> '' \
                 ORDER BY id LIMIT 1",
                params![account_id.as_str(), kind.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(sql_err_apply)?
        } else {
            conn.query_row(
                "SELECT remote_id FROM folders WHERE account_id = ?1 AND id = ?2 LIMIT 1",
                params![account_id.as_str(), folder_key],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(sql_err_apply)?
        };

        remote_id
            .flatten()
            .filter(|s| !s.is_empty())
            .ok_or(ApplyError::Unsupported)
    }

    fn remote_folder_for_operation(
        &self,
        account_id: &AccountId,
        folder_key: &str,
        operation_id: &str,
    ) -> Result<String, ApplyError> {
        let conn = self.db.conn();
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT om.destination_remote_id
                 FROM operation_moves om
                 JOIN operations o ON o.id = om.operation_id
                 WHERE om.operation_id = ?1 AND o.account_id = ?2
                   AND o.status IN ('pending', 'running')",
            )
            .map_err(sql_err_apply)?;
        let destinations = stmt
            .query_map(params![operation_id, account_id.as_str()], |row| {
                row.get::<_, String>(0)
            })
            .map_err(sql_err_apply)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err_apply)?;
        let mut unique = destinations.into_iter().filter(|s| !s.is_empty());
        if let Some(destination) = unique.next() {
            if unique.any(|other| other != destination) {
                return Err(ApplyError::Conflict);
            }
            return Ok(destination);
        }
        self.remote_folder(account_id, folder_key)
    }
}

/// Reads back the `{"targets":[{"folder":"…","uid":N},…]}` snapshot
/// `store::permanent_delete_payload` writes at dispatch. Mirror image of
/// that writer (only `\` and `"` are ever escaped), in the same
/// dependency-free spirit as `store::json_string_field`.
///
/// Anything malformed stops the scan and keeps what parsed so far only up to
/// that point: a truncated payload must not be rounded up into "delete the
/// whole thread".
fn permanent_delete_targets(payload: &str) -> Vec<RemoteMessage> {
    const FOLDER: &str = "\"folder\":\"";
    const UID: &str = "\"uid\":";
    let mut out = Vec::new();
    let Some((_, mut rest)) = payload.split_once("\"targets\":[") else {
        return out;
    };
    while let Some(at) = rest.find(FOLDER) {
        let value_at = at + FOLDER.len();
        let mut chars = rest[value_at..].char_indices();
        let mut folder = String::new();
        let end = loop {
            let Some((i, c)) = chars.next() else {
                return out;
            };
            match c {
                '\\' => match chars.next() {
                    Some((_, escaped)) => folder.push(escaped),
                    None => return out,
                },
                '"' => break value_at + i + 1,
                _ => folder.push(c),
            }
        };
        rest = &rest[end..];
        let Some(uid_at) = rest.find(UID) else {
            return out;
        };
        let digits: String = rest[uid_at + UID.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        rest = &rest[uid_at + UID.len() + digits.len()..];
        match digits.parse::<u32>() {
            Ok(uid) if !folder.is_empty() => out.push(RemoteMessage { folder, uid }),
            _ => return out,
        }
    }
    out
}

/// Maps a well-known [`RemoteLocator::remote_folder`] key to the
/// [`FolderKind`] it resolves through. `None` means `folder_key` is a real
/// local `folders.id` instead (the `Move` case).
fn system_kind(folder_key: &str) -> Option<FolderKind> {
    match folder_key {
        ARCHIVE_FOLDER_KEY => Some(FolderKind::Archive),
        TRASH_FOLDER_KEY => Some(FolderKind::Trash),
        _ => None,
    }
}

/// Local SQLite failure while resolving IMAP coordinates. `ApplyError` has
/// no "local storage broke" variant (D9 keeps this trait's vocabulary
/// wire-shaped), and `Unsupported` is the closest available answer: the
/// operation fails rather than reporting success or inventing
/// coordinates.
///
/// Be clear about what that costs, though. `ApplyError::retry()` is true
/// only for `Network`, so this fails the operation *permanently* -- and a
/// SQLite error here is quite often transient (`SQLITE_BUSY` past
/// `busy_timeout`, which D13's WAL setup makes a normal occurrence once
/// more than one handle is writing). A momentary lock therefore ends the
/// operation for good, while the thread is already marked locally. That
/// is the same defect T-081 exists to settle -- deliberately not patched
/// over here with a `Network` that would misreport a local failure as a
/// network one.
fn sql_err_apply(_: rusqlite::Error) -> ApplyError {
    ApplyError::Unsupported
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Core;

    /// Seeds one account with one real folder of the given `kind`/`id`/
    /// `remote_id`. Mirrors `body::tests::seed_message`'s minimal-insert
    /// style (that helper lives in a module this task isn't allowed to
    /// touch, so this is a small local copy, not a shared import).
    fn seed_account(core: &Core, account: &str) {
        let conn = core.db.conn();
        conn.execute(
            "INSERT OR IGNORE INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
             VALUES (?1, ?1, 'a@example.com', 'generic', 'synced', 'recent', 0, 0)",
            params![account],
        )
        .unwrap();
    }

    fn seed_folder(
        core: &Core,
        account: &str,
        id: &str,
        kind: FolderKind,
        remote_id: Option<&str>,
    ) {
        seed_account(core, account);
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, remote_id, name, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, account, remote_id, id, kind.as_str()],
            )
            .unwrap();
    }

    fn seed_message(
        core: &Core,
        account: &str,
        thread_id: &str,
        msg_id: &str,
        folder_id: &str,
        provider_uid: Option<u32>,
    ) {
        core.db
            .conn()
            .execute(
                "INSERT OR IGNORE INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
                 VALUES (?1, ?2, ?3, 'Hello', 'Hi', 0, 0)",
                params![thread_id, account, folder_id],
            )
            .unwrap();
        core.db
            .conn()
            .execute(
                "INSERT INTO messages (id, account_id, thread_id, folder_id, date, provider_uid)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                params![msg_id, account, thread_id, folder_id, provider_uid],
            )
            .unwrap();
    }

    // --- remote_folder: system keys resolve by kind, not by name ---

    #[test]
    fn archive_key_resolves_by_kind_even_when_folder_is_not_named_archive() {
        let core = Core::memory().unwrap();
        // The server calls its Archive-kind mailbox something that does
        // not literally say "archive" anywhere -- proves the lookup goes
        // through `folders.kind`, not a guessed/matched name.
        seed_folder(
            &core,
            "acc1",
            "acc1:allmail",
            FolderKind::Archive,
            Some("[Gmail]/All Mail"),
        );

        let resolved = core
            .remote_folder(&AccountId("acc1".into()), ARCHIVE_FOLDER_KEY)
            .unwrap();
        assert_eq!(resolved, "[Gmail]/All Mail");
    }

    /// An account can end up with two folders of the same system kind:
    /// a placeholder T-077 could not adopt (its `remote_id` still NULL)
    /// next to the real discovered mailbox. Resolution must find the one
    /// that actually resolves, whichever row happens to come first.
    #[test]
    fn a_second_unresolved_folder_of_the_same_kind_does_not_hide_the_real_one() {
        let core = Core::memory().unwrap();
        // Seeded first on purpose: an unqualified `LIMIT 1` would take
        // this row and refuse the operation.
        seed_folder(&core, "acc1", "acc1:archive", FolderKind::Archive, None);
        seed_folder(
            &core,
            "acc1",
            "acc1:zz-allmail",
            FolderKind::Archive,
            Some("[Gmail]/All Mail"),
        );

        let resolved = core
            .remote_folder(&AccountId("acc1".into()), ARCHIVE_FOLDER_KEY)
            .unwrap();
        assert_eq!(resolved, "[Gmail]/All Mail");
    }

    #[test]
    fn trash_key_resolves_by_kind() {
        let core = Core::memory().unwrap();
        seed_folder(&core, "acc1", "acc1:bin", FolderKind::Trash, Some("Bin"));

        let resolved = core
            .remote_folder(&AccountId("acc1".into()), TRASH_FOLDER_KEY)
            .unwrap();
        assert_eq!(resolved, "Bin");
    }

    #[test]
    fn move_folder_key_resolves_by_local_folder_id_not_kind() {
        let core = Core::memory().unwrap();
        seed_folder(
            &core,
            "acc1",
            "acc1:projects",
            FolderKind::Custom,
            Some("Projects"),
        );

        let resolved = core
            .remote_folder(&AccountId("acc1".into()), "acc1:projects")
            .unwrap();
        assert_eq!(resolved, "Projects");
    }

    #[test]
    fn no_folder_of_that_kind_is_unsupported() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1");
        // No Archive-kind folder exists for this account at all.
        let err = core
            .remote_folder(&AccountId("acc1".into()), ARCHIVE_FOLDER_KEY)
            .unwrap_err();
        assert_eq!(err, ApplyError::Unsupported);
    }

    #[test]
    fn unmatched_remote_id_is_unsupported_not_a_guessed_name() {
        let core = Core::memory().unwrap();
        // Real Archive-kind row exists, but T-077 has never matched it
        // against a server LIST yet.
        seed_folder(&core, "acc1", "acc1:allmail", FolderKind::Archive, None);

        let err = core
            .remote_folder(&AccountId("acc1".into()), ARCHIVE_FOLDER_KEY)
            .unwrap_err();
        assert_eq!(err, ApplyError::Unsupported);
    }

    #[test]
    fn empty_remote_id_is_unsupported_same_as_null() {
        let core = Core::memory().unwrap();
        seed_folder(&core, "acc1", "acc1:allmail", FolderKind::Archive, Some(""));

        let err = core
            .remote_folder(&AccountId("acc1".into()), ARCHIVE_FOLDER_KEY)
            .unwrap_err();
        assert_eq!(err, ApplyError::Unsupported);
    }

    // --- thread_messages ---

    #[test]
    fn thread_in_two_folders_returns_both_pairs() {
        let core = Core::memory().unwrap();
        seed_folder(
            &core,
            "acc1",
            "acc1:inbox",
            FolderKind::Inbox,
            Some("INBOX"),
        );
        seed_folder(
            &core,
            "acc1",
            "acc1:allmail",
            FolderKind::Archive,
            Some("[Gmail]/All Mail"),
        );
        // A partially-landed move (T-075's doc comment scenario): one
        // message row still in INBOX, one already in Archive, same thread.
        seed_message(&core, "acc1", "t1", "m1", "acc1:inbox", Some(7));
        seed_message(&core, "acc1", "t1", "m2", "acc1:allmail", Some(9));

        let mut messages = core
            .thread_messages(&AccountId("acc1".into()), "t1")
            .unwrap();
        messages.sort_by_key(|m| m.uid);

        assert_eq!(
            messages,
            vec![
                RemoteMessage {
                    folder: "INBOX".into(),
                    uid: 7,
                },
                RemoteMessage {
                    folder: "[Gmail]/All Mail".into(),
                    uid: 9,
                },
            ]
        );
    }

    #[test]
    fn overlay_archive_does_not_turn_into_a_remote_move_after_folder_discovery() {
        let mut core = Core::memory().unwrap();
        seed_folder(
            &core,
            "acc1",
            "acc1:inbox",
            FolderKind::Inbox,
            Some("INBOX"),
        );
        seed_message(&core, "acc1", "t1", "m1", "acc1:inbox", Some(7));
        let operation_id = core
            .dispatch_with_receipt(crate::command::Command::Archive {
                account_id: AccountId("acc1".into()),
                thread_ids: vec![crate::model::ThreadId("t1".into())],
            })
            .unwrap()
            .operations
            .into_iter()
            .next()
            .expect("archive overlay still queues a provider operation")
            .operation_id;

        // A LIST may discover a real Archive mailbox while the operation is
        // waiting.  The overlay operation has no captured intent, so it must
        // fail closed instead of moving the message to this newly discovered
        // mailbox by accident.
        seed_folder(
            &core,
            "acc1",
            "acc1:archive",
            FolderKind::Archive,
            Some("Archive"),
        );
        assert!(core
            .thread_messages_for_operation(&AccountId("acc1".into()), "t1", operation_id.as_str())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn move_to_the_current_real_folder_does_not_leave_an_intent() {
        let mut core = Core::memory().unwrap();
        seed_folder(
            &core,
            "acc1",
            "acc1:inbox",
            FolderKind::Inbox,
            Some("INBOX"),
        );
        seed_message(&core, "acc1", "t1", "m1", "acc1:inbox", Some(7));
        let receipt = core
            .dispatch_with_receipt(crate::command::Command::Move {
                account_id: AccountId("acc1".into()),
                thread_ids: vec![crate::model::ThreadId("t1".into())],
                folder_id: crate::model::FolderId("acc1:inbox".into()),
            })
            .unwrap();
        assert!(receipt.operations.is_empty());
        let intents: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM operation_moves", [], |row| row.get(0))
            .unwrap();
        assert_eq!(intents, 0);
    }

    #[test]
    fn move_without_durable_intent_keeps_the_legacy_locator_fallback() {
        let mut core = Core::memory().unwrap();
        seed_folder(
            &core,
            "acc1",
            "acc1:inbox",
            FolderKind::Inbox,
            Some("INBOX"),
        );
        // The destination exists locally but has not been matched to a
        // server mailbox yet. This is the pre-T-076 Move path: dispatch must
        // still queue the operation, and a later folder discovery must not
        // lose the source coordinates that the provider needs.
        seed_folder(&core, "acc1", "acc1:projects", FolderKind::Custom, None);
        seed_message(&core, "acc1", "t1", "m1", "acc1:inbox", Some(7));
        let operation_id = core
            .dispatch_with_receipt(crate::command::Command::Move {
                account_id: AccountId("acc1".into()),
                thread_ids: vec![crate::model::ThreadId("t1".into())],
                folder_id: crate::model::FolderId("acc1:projects".into()),
            })
            .unwrap()
            .operations
            .into_iter()
            .next()
            .expect("unresolved Move still queues a provider operation")
            .operation_id;

        // The destination is discovered after dispatch, before the worker
        // gets to the operation. The legacy path must continue to locate the
        // physical source row, while remote_folder_for_operation resolves
        // the newly known destination separately.
        core.db
            .conn()
            .execute(
                "UPDATE folders SET remote_id = 'Projects' WHERE id = 'acc1:projects'",
                [],
            )
            .unwrap();

        assert_eq!(
            core.thread_messages_for_operation(
                &AccountId("acc1".into()),
                "t1",
                operation_id.as_str()
            )
            .unwrap(),
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }]
        );
    }

    #[test]
    fn thread_messages_skips_rows_with_no_provider_uid_or_folder_remote_id() {
        let core = Core::memory().unwrap();
        seed_folder(
            &core,
            "acc1",
            "acc1:inbox",
            FolderKind::Inbox,
            Some("INBOX"),
        );
        seed_folder(&core, "acc1", "acc1:drafts", FolderKind::Drafts, None);
        // Not yet synced (no provider_uid): must not be reported as an
        // actionable coordinate.
        seed_message(&core, "acc1", "t1", "m1", "acc1:inbox", None);
        // Folder has no remote_id yet either: same rule applies.
        seed_message(&core, "acc1", "t1", "m2", "acc1:drafts", Some(3));
        // The one resolvable row.
        seed_message(&core, "acc1", "t1", "m3", "acc1:inbox", Some(7));

        let messages = core
            .thread_messages(&AccountId("acc1".into()), "t1")
            .unwrap();
        assert_eq!(
            messages,
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }]
        );
    }

    #[test]
    fn unknown_thread_is_an_empty_list_not_an_error() {
        let core = Core::memory().unwrap();
        seed_account(&core, "acc1");
        let messages = core
            .thread_messages(&AccountId("acc1".into()), "does-not-exist")
            .unwrap();
        assert!(messages.is_empty());
    }

    // --- PermanentDelete resolves the snapshot taken at dispatch ---

    fn queue_permanent_delete(core: &mut Core, thread: &str) -> String {
        core.dispatch_with_receipt(crate::command::Command::PermanentDelete {
            account_id: AccountId("acc1".into()),
            thread_ids: vec![crate::model::ThreadId(thread.into())],
        })
        .unwrap()
        .operations
        .into_iter()
        .next()
        .expect("PermanentDelete queues a provider operation")
        .operation_id
        .as_str()
        .to_string()
    }

    /// `UID EXPUNGE` cannot be undone, so the targets are whatever the thread
    /// held when the user pressed the button -- not whatever it holds when
    /// the worker finally gets to the operation. A reply that lands while the
    /// operation waits (offline, or in backoff) must survive.
    #[test]
    fn permanent_delete_targets_are_frozen_when_the_command_is_queued() {
        let mut core = Core::memory().unwrap();
        seed_folder(
            &core,
            "acc1",
            "acc1:inbox",
            FolderKind::Inbox,
            Some("INBOX"),
        );
        seed_message(&core, "acc1", "t1", "m1", "acc1:inbox", Some(7));
        let operation_id = queue_permanent_delete(&mut core, "t1");

        // Sync brings a reply on the same thread after the command was queued.
        seed_message(&core, "acc1", "t1", "m2", "acc1:inbox", Some(999));

        let account = AccountId("acc1".into());
        assert_eq!(
            core.thread_messages_for_operation(&account, "t1", &operation_id)
                .unwrap(),
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }],
            "the snapshot must not grow after dispatch"
        );
        assert_eq!(
            core.thread_messages(&account, "t1").unwrap().len(),
            2,
            "the live view is unchanged: only PermanentDelete freezes"
        );
    }

    /// A mailbox name with a quote or a backslash has to survive the payload
    /// round trip, or the EXPUNGE would be aimed at a different mailbox.
    #[test]
    fn a_snapshot_survives_a_mailbox_name_that_needs_escaping() {
        let mut core = Core::memory().unwrap();
        seed_folder(
            &core,
            "acc1",
            "acc1:odd",
            FolderKind::Inbox,
            Some(r#"Q1 "plan"\draft"#),
        );
        seed_message(&core, "acc1", "t1", "m1", "acc1:odd", Some(7));
        let operation_id = queue_permanent_delete(&mut core, "t1");
        assert_eq!(
            core.thread_messages_for_operation(&AccountId("acc1".into()), "t1", &operation_id)
                .unwrap(),
            vec![RemoteMessage {
                folder: r#"Q1 "plan"\draft"#.into(),
                uid: 7,
            }]
        );
    }

    /// An operation queued before Core started writing the snapshot resolves
    /// to nothing, so `ImapMailProvider` answers `NotFound` instead of
    /// expunging a thread nobody described.
    #[test]
    fn a_permanent_delete_with_no_snapshot_resolves_to_nothing() {
        let mut core = Core::memory().unwrap();
        seed_folder(
            &core,
            "acc1",
            "acc1:inbox",
            FolderKind::Inbox,
            Some("INBOX"),
        );
        seed_message(&core, "acc1", "t1", "m1", "acc1:inbox", Some(7));
        let operation_id = queue_permanent_delete(&mut core, "t1");
        core.db
            .conn()
            .execute(
                "UPDATE operations SET payload = '{}' WHERE id = ?1",
                params![operation_id],
            )
            .unwrap();
        assert!(core
            .thread_messages_for_operation(&AccountId("acc1".into()), "t1", &operation_id)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_truncated_snapshot_never_widens_into_the_whole_thread() {
        assert!(permanent_delete_targets("{}").is_empty());
        assert!(permanent_delete_targets(r#"{"targets":[]}"#).is_empty());
        assert_eq!(
            permanent_delete_targets(r#"{"targets":[{"folder":"INBOX","uid":7},{"folder":"Sen"#),
            vec![RemoteMessage {
                folder: "INBOX".into(),
                uid: 7,
            }]
        );
    }
}
