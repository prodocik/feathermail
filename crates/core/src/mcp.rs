//! Durable MCP policy and GTK confirmation hand-off (T-059).
//!
//! The stdio process and GTK window are separate local processes, so neither
//! can borrow the other's state.  This module keeps the authority in Core and
//! records only opaque ids, capabilities and revisions in SQLite.  It never
//! accepts a tool's arguments, bodies, paths or credentials for persistence.

use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::command::{Command, MailEvent};
use crate::error::{CoreError, ErrorCode};
use crate::model::{AccountId, DraftId, FolderId, OperationId, ThreadId};
use crate::store::{
    delete_folder_in, dispatch_with_receipt_in, queue_draft_send_in, sql_err, Core, DispatchReceipt,
};

/// Upper bounds are ordered so a caller can only narrow a stored policy.
/// Draft includes ordinary draft, W and M actions; Send and Full additionally
/// make a request eligible to ask for their respective high-risk action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum McpPermissionLevel {
    Read,
    #[default]
    Draft,
    Send,
    Full,
}

impl McpPermissionLevel {
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_lowercase().as_str() {
            "draft" | "read+draft" => Self::Draft,
            "send" | "read+draft+send" => Self::Send,
            "full" => Self::Full,
            _ => Self::Read,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Draft => "draft",
            Self::Send => "send",
            Self::Full => "full",
        }
    }
}

/// Safe, durable policy metadata for one enrolled local MCP profile.  This
/// deliberately omits grants, requests, audit rows, tool arguments, and all
/// mail data so Settings can render client status without becoming a policy
/// authority itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpClientSummary {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub permission_level: McpPermissionLevel,
}

/// The three choices exposed by the GTK confirmation dialog.  "Always" is
/// a per-tool grant; it never widens the launch process's environment ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpConfirmationChoice {
    Deny,
    AllowOnce,
    AlwaysAllow,
}

/// Metadata that GTK may show while asking its user.  There is deliberately
/// no argument, subject, body, filesystem path, token or password field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpConfirmationRequest {
    pub id: i64,
    pub client_id: String,
    pub capability: String,
    pub account_id: Option<AccountId>,
    pub target_id: Option<String>,
    /// Safe scalar summary for a bounded batch.  This is never an id or an
    /// argument list, and lets GTK describe a batch without exposing mail
    /// metadata.
    pub target_count: u32,
    pub expires_at: i64,
}

/// The only results the MCP boundary can receive from Core policy evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpAuthorization {
    Allowed,
    NeedsConfirmation(McpConfirmationRequest),
    Denied,
}

/// Result of the specialised MCP Send doorway.  Unlike a general tool
/// authorization, this holds the high-risk approval, draft revision check,
/// outbox freeze and operation enqueue in one SQLite transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpSendOutcome {
    Queued(OperationId),
    NeedsConfirmation(McpConfirmationRequest),
    Denied,
}

/// Result of a specialised high-risk batch MCP doorway. Core owns both the
/// exact batch fingerprint and the transaction that consumes Allow-once before
/// it makes the local queued mutation visible.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpBulkHighRiskOutcome {
    Queued(DispatchReceipt),
    NeedsConfirmation(McpConfirmationRequest),
    Denied,
}

/// Result of the T-060u folder-deletion door. `queued` repeats
/// [`Core::delete_folder`]'s own return value: the folder is gone locally
/// either way, and `false` only means there was no server mailbox to
/// delete because the folder had never been created on one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpFolderDeleteOutcome {
    Deleted { queued: bool },
    NeedsConfirmation(McpConfirmationRequest),
    Denied,
}

const CONFIRMATION_TTL_SECS: i64 = 120;
const MAX_CLIENT_ID_BYTES: usize = 128;
const MAX_CAPABILITY_BYTES: usize = 128;
const MAX_TARGET_ID_BYTES: usize = 256;
const MAX_FINGERPRINT_BYTES: usize = 384;
const MAX_MCP_HIGH_RISK_BATCH_TARGETS: usize = 100;

impl Core {
    /// The only MCP door that can Trash a batch.  The caller supplies typed
    /// ids, but not an approval fingerprint: Core canonicalises and hashes
    /// the exact set itself, records only the safe count alongside that
    /// digest, and consumes a matching Allow-once in the same immediate
    /// SQLite transaction as the vector [`Command::Trash`] dispatch.
    pub fn queue_mcp_bulk_trash(
        &mut self,
        client_id: &str,
        ceiling: McpPermissionLevel,
        account_id: &AccountId,
        thread_ids: Vec<ThreadId>,
    ) -> Result<McpBulkHighRiskOutcome, CoreError> {
        self.queue_mcp_high_risk_batch(
            client_id,
            ceiling,
            "bulk_delete",
            account_id,
            Command::Trash {
                account_id: account_id.clone(),
                thread_ids,
            },
        )
    }

    /// The only MCP door that can permanently delete a batch. Like Trash,
    /// this binds a GTK decision to the exact unordered set and joins its
    /// Allow-once consume to the vector [`Command::PermanentDelete`] dispatch.
    pub fn queue_mcp_bulk_permanent_delete(
        &mut self,
        client_id: &str,
        ceiling: McpPermissionLevel,
        account_id: &AccountId,
        thread_ids: Vec<ThreadId>,
    ) -> Result<McpBulkHighRiskOutcome, CoreError> {
        self.queue_mcp_high_risk_batch(
            client_id,
            ceiling,
            "bulk_permanent_delete",
            account_id,
            Command::PermanentDelete {
                account_id: account_id.clone(),
                thread_ids,
            },
        )
    }

    /// Shared implementation for the two explicitly supported irreversible
    /// vector actions. `capability` is a private static literal selected by
    /// the wrappers above, never a caller-provided tool name or argument.
    fn queue_mcp_high_risk_batch(
        &mut self,
        client_id: &str,
        ceiling: McpPermissionLevel,
        capability: &'static str,
        account_id: &AccountId,
        cmd: Command,
    ) -> Result<McpBulkHighRiskOutcome, CoreError> {
        if !valid_identifier(client_id, MAX_CLIENT_ID_BYTES) {
            return Ok(McpBulkHighRiskOutcome::Denied);
        }
        let thread_ids = cmd.thread_ids();
        let (fingerprint, target_count) =
            bulk_high_risk_fingerprint(capability, account_id, &thread_ids)?;
        let now = self.now();
        let tx = self.db.immediate_transaction().map_err(sql_err)?;
        let decision = high_risk_gate(
            &tx,
            HighRiskGateInput {
                client_id,
                ceiling,
                capability,
                account_id,
                target_id: None,
                target_count,
                fingerprint: &fingerprint,
            },
            now,
        )?;
        match decision {
            HighRiskDecision::Denied => Ok(McpBulkHighRiskOutcome::Denied),
            HighRiskDecision::NeedsConfirmation(request) => {
                tx.commit().map_err(sql_err)?;
                Ok(McpBulkHighRiskOutcome::NeedsConfirmation(request))
            }
            HighRiskDecision::Proceed => {
                let receipt = dispatch_with_receipt_in(&tx, &cmd, now)?;
                tx.commit().map_err(sql_err)?;
                self.emit(MailEvent::ThreadsChanged {
                    account_id: account_id.clone(),
                    thread_ids: thread_ids.clone(),
                });
                Ok(McpBulkHighRiskOutcome::Queued(receipt))
            }
        }
    }

    /// T-060u: the only MCP door that can delete a folder, and the first
    /// high-risk door whose target is not a set of threads.
    ///
    /// A folder deletion is irreversible on the server and cannot be
    /// undone from the Undo history, so it sits at the same permission
    /// height as a bulk permanent delete: `Full` ceiling, `Full` stored
    /// level, and a GTK confirmation bound to *this* account and *this*
    /// folder. The approval is consumed in the same transaction as the
    /// deletion, so an approval can never be spent on nothing, and a
    /// deletion can never happen on a spent approval.
    ///
    /// Core still applies every rule [`Core::delete_folder`] applies --
    /// custom folders only, empty folders only. An agent holding a valid
    /// one-shot approval for a folder that filled up in the meantime gets
    /// the same refusal a user would.
    pub fn queue_mcp_delete_folder(
        &mut self,
        client_id: &str,
        ceiling: McpPermissionLevel,
        account_id: &AccountId,
        folder_id: &FolderId,
    ) -> Result<McpFolderDeleteOutcome, CoreError> {
        if !valid_identifier(client_id, MAX_CLIENT_ID_BYTES) {
            return Ok(McpFolderDeleteOutcome::Denied);
        }
        let fingerprint = folder_high_risk_fingerprint("delete_folder", account_id, folder_id)?;
        let now = self.now();
        let tx = self.db.immediate_transaction().map_err(sql_err)?;
        let decision = high_risk_gate(
            &tx,
            HighRiskGateInput {
                client_id,
                ceiling,
                capability: "delete_folder",
                account_id,
                target_id: Some(folder_id.as_str()),
                target_count: 1,
                fingerprint: &fingerprint,
            },
            now,
        )?;
        match decision {
            HighRiskDecision::Denied => Ok(McpFolderDeleteOutcome::Denied),
            HighRiskDecision::NeedsConfirmation(request) => {
                tx.commit().map_err(sql_err)?;
                Ok(McpFolderDeleteOutcome::NeedsConfirmation(request))
            }
            HighRiskDecision::Proceed => {
                let queued = delete_folder_in(&tx, account_id.as_str(), folder_id.as_str(), now)?;
                tx.commit().map_err(sql_err)?;
                // No event: `MailEvent` speaks only about threads, and
                // `Core::create_folder`/`rename_folder` are silent for the
                // same reason. The sidebar refreshes on its own next read.
                Ok(McpFolderDeleteOutcome::Deleted { queued })
            }
        }
    }

    /// The only MCP door that can queue a send.  It does not accept message
    /// contents or a caller-supplied fingerprint: Core reads only the current
    /// integer draft revision inside its transaction, consumes a matching
    /// Allow-once request there, then freezes exactly that revision before
    /// committing.  Thus a stale dialog cannot queue an edited draft.
    /// Read-only mirror of the ceiling predicate every mutating MCP door
    /// applies inside its own transaction: MCP on, this client enabled, and
    /// its durable Settings level at or above `required`.
    ///
    /// It exists so a tool that has to write something locally before it can
    /// reach its Core door (`send_email` saves the draft it is about to send)
    /// can refuse a client the durable policy would refuse anyway. It is a
    /// pre-check, never the decision: the write door re-reads the same rows
    /// under its own transaction and remains authoritative.
    pub fn mcp_client_allows(
        &self,
        client_id: &str,
        required: McpPermissionLevel,
    ) -> Result<bool, CoreError> {
        if !valid_identifier(client_id, MAX_CLIENT_ID_BYTES) {
            return Ok(false);
        }
        let conn = self.db.conn();
        if !mcp_enabled_in(conn)? {
            return Ok(false);
        }
        let client: Option<(i64, String)> = conn
            .query_row(
                "SELECT enabled, permission_level FROM mcp_clients WHERE id = ?1",
                params![client_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((enabled, stored_level)) = client else {
            return Ok(false);
        };
        Ok(enabled == 1 && McpPermissionLevel::parse(&stored_level) >= required)
    }

    pub fn queue_mcp_draft_send(
        &mut self,
        client_id: &str,
        ceiling: McpPermissionLevel,
        account_id: &AccountId,
        draft_id: &DraftId,
    ) -> Result<McpSendOutcome, CoreError> {
        if !valid_identifier(client_id, MAX_CLIENT_ID_BYTES)
            || !valid_identifier(draft_id.as_str(), MAX_TARGET_ID_BYTES)
        {
            return Ok(McpSendOutcome::Denied);
        }

        let now = self.now();
        let tx = self.db.conn().unchecked_transaction().map_err(sql_err)?;
        if !mcp_enabled_in(&tx)? || ceiling < McpPermissionLevel::Send {
            return Ok(McpSendOutcome::Denied);
        }
        let client: Option<(i64, String)> = tx
            .query_row(
                "SELECT enabled, permission_level FROM mcp_clients WHERE id = ?1",
                params![client_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((enabled, stored_level)) = client else {
            return Ok(McpSendOutcome::Denied);
        };
        if enabled != 1 || McpPermissionLevel::parse(&stored_level) < McpPermissionLevel::Send {
            return Ok(McpSendOutcome::Denied);
        }

        let revision: i64 = tx
            .query_row(
                "SELECT sync_revision FROM drafts WHERE account_id = ?1 AND id = ?2",
                params![account_id.as_str(), draft_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?
            .ok_or_else(|| CoreError::from_code(ErrorCode::MessageNotFound))?;
        let fingerprint = format!("send:{}:{revision}", draft_id.as_str());
        let grant: Option<String> = tx
            .query_row(
                "SELECT grant FROM mcp_permissions
                 WHERE client_id = ?1 AND capability = 'send_draft'",
                params![client_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match grant.as_deref() {
            Some("deny") => return Ok(McpSendOutcome::Denied),
            Some("allow") => {
                let operation =
                    queue_draft_send_in(&tx, account_id, draft_id, Some(revision), now)?;
                tx.commit().map_err(sql_err)?;
                return Ok(McpSendOutcome::Queued(operation));
            }
            _ => {}
        }

        tx.execute(
            "UPDATE mcp_confirmation_requests
             SET status = 'expired', resolved_at = ?1
             WHERE status = 'pending' AND expires_at <= ?1",
            params![now],
        )
        .map_err(sql_err)?;
        let existing: Option<(i64, String, i64)> = tx
            .query_row(
                "SELECT id, status, expires_at FROM mcp_confirmation_requests
                 WHERE client_id = ?1 AND capability = 'send_draft' AND fingerprint = ?2
                   AND status IN ('pending', 'allow_once', 'denied')
                 ORDER BY id DESC LIMIT 1",
                params![client_id, fingerprint],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((id, status, expires_at)) = existing {
            match status.as_str() {
                "allow_once" if expires_at > now => {
                    let consumed = tx
                        .execute(
                            "UPDATE mcp_confirmation_requests
                             SET status = 'consumed', resolved_at = ?2
                             WHERE id = ?1 AND status = 'allow_once' AND expires_at > ?2",
                            params![id, now],
                        )
                        .map_err(sql_err)?;
                    if consumed != 1 {
                        return Ok(McpSendOutcome::Denied);
                    }
                    let operation =
                        queue_draft_send_in(&tx, account_id, draft_id, Some(revision), now)?;
                    tx.commit().map_err(sql_err)?;
                    return Ok(McpSendOutcome::Queued(operation));
                }
                "pending" if expires_at > now => {
                    tx.commit().map_err(sql_err)?;
                    return Ok(McpSendOutcome::NeedsConfirmation(McpConfirmationRequest {
                        id,
                        client_id: client_id.into(),
                        capability: "send_draft".into(),
                        account_id: Some(account_id.clone()),
                        target_id: Some(draft_id.as_str().into()),
                        target_count: 1,
                        expires_at,
                    }));
                }
                _ => return Ok(McpSendOutcome::Denied),
            }
        }

        let expires_at = now.saturating_add(CONFIRMATION_TTL_SECS);
        tx.execute(
            "INSERT INTO mcp_confirmation_requests
             (client_id, capability, account_id, target_id, fingerprint, status, created_at, expires_at)
             VALUES (?1, 'send_draft', ?2, ?3, ?4, 'pending', ?5, ?6)",
            params![
                client_id,
                account_id.as_str(),
                draft_id.as_str(),
                fingerprint,
                now,
                expires_at
            ],
        )
        .map_err(sql_err)?;
        let id = tx.last_insert_rowid();
        tx.commit().map_err(sql_err)?;
        Ok(McpSendOutcome::NeedsConfirmation(McpConfirmationRequest {
            id,
            client_id: client_id.into(),
            capability: "send_draft".into(),
            account_id: Some(account_id.clone()),
            target_id: Some(draft_id.as_str().into()),
            target_count: 1,
            expires_at,
        }))
    }

    /// Resolve one MCP tool's access using the live persisted on/off setting,
    /// its durable per-client policy and the caller's environment ceiling.
    ///
    /// `ceiling` and an account allowlist are constraints supplied by the
    /// launching environment; neither can create a grant.  A high-risk tool
    /// gets an opaque durable request unless its exact per-tool grant says
    /// `allow`.  An Allow-once row is atomically consumed on the next matching
    /// authorization attempt, so retries cannot replay it.
    #[allow(clippy::too_many_arguments)]
    pub fn authorize_mcp_action(
        &mut self,
        client_id: &str,
        ceiling: McpPermissionLevel,
        capability: &str,
        required_level: McpPermissionLevel,
        needs_confirmation: bool,
        account_id: Option<&AccountId>,
        target_id: Option<&str>,
        fingerprint: &str,
    ) -> Result<McpAuthorization, CoreError> {
        if !valid_identifier(client_id, MAX_CLIENT_ID_BYTES)
            || !valid_identifier(capability, MAX_CAPABILITY_BYTES)
            || !valid_identifier(fingerprint, MAX_FINGERPRINT_BYTES)
            || target_id.is_some_and(|id| !valid_identifier(id, MAX_TARGET_ID_BYTES))
        {
            return Ok(McpAuthorization::Denied);
        }
        // Generic High actions are single-target only.  A vector action must
        // use a specialised Core doorway that derives and consumes its own
        // exact batch fingerprint with the mutation (T-060l).
        if needs_confirmation && target_id.is_none() {
            return Ok(McpAuthorization::Denied);
        }

        let now = self.now();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        if !mcp_enabled_in(&tx)? {
            return Ok(McpAuthorization::Denied);
        }
        let client: Option<(i64, String)> = tx
            .query_row(
                "SELECT enabled, permission_level FROM mcp_clients WHERE id = ?1",
                params![client_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((enabled, stored_level)) = client else {
            return Ok(McpAuthorization::Denied);
        };
        // Both the launch environment and the durable Settings policy are
        // ceilings. Neither an existing per-tool grant nor a GTK decision
        // may widen the persisted level after the user lowers it.
        if enabled == 0
            || ceiling < required_level
            || McpPermissionLevel::parse(&stored_level) < required_level
        {
            return Ok(McpAuthorization::Denied);
        }

        let grant: Option<String> = tx
            .query_row(
                "SELECT grant FROM mcp_permissions
                 WHERE client_id = ?1 AND capability = ?2",
                params![client_id, capability],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        match grant.as_deref() {
            Some("deny") => return Ok(McpAuthorization::Denied),
            Some("allow") => {
                tx.commit().map_err(sql_err)?;
                return Ok(McpAuthorization::Allowed);
            }
            _ => {}
        }

        if !needs_confirmation {
            tx.commit().map_err(sql_err)?;
            return Ok(McpAuthorization::Allowed);
        }

        // The persisted level only makes a high-risk action eligible to ask.
        // Prune expired requests before checking whether this exact request
        // was resolved, so an old dialog can never authorize a later action.
        tx.execute(
            "UPDATE mcp_confirmation_requests
             SET status = 'expired', resolved_at = ?1
             WHERE status = 'pending' AND expires_at <= ?1",
            params![now],
        )
        .map_err(sql_err)?;
        let existing: Option<(i64, String, i64)> = tx
            .query_row(
                "SELECT id, status, expires_at FROM mcp_confirmation_requests
                 WHERE client_id = ?1 AND capability = ?2 AND fingerprint = ?3
                   AND status IN ('pending', 'allow_once', 'denied')
                 ORDER BY id DESC LIMIT 1",
                params![client_id, capability, fingerprint],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sql_err)?;
        if let Some((id, status, expires_at)) = existing {
            match status.as_str() {
                "allow_once" if expires_at > now => {
                    let consumed = tx
                        .execute(
                            "UPDATE mcp_confirmation_requests
                             SET status = 'consumed', resolved_at = ?2
                             WHERE id = ?1 AND status = 'allow_once' AND expires_at > ?2",
                            params![id, now],
                        )
                        .map_err(sql_err)?;
                    if consumed == 1 {
                        tx.commit().map_err(sql_err)?;
                        return Ok(McpAuthorization::Allowed);
                    }
                    return Ok(McpAuthorization::Denied);
                }
                "pending" if expires_at > now => {
                    tx.commit().map_err(sql_err)?;
                    return Ok(McpAuthorization::NeedsConfirmation(
                        McpConfirmationRequest {
                            id,
                            client_id: client_id.into(),
                            capability: capability.into(),
                            account_id: account_id.cloned(),
                            target_id: target_id.map(str::to_owned),
                            target_count: 1,
                            expires_at,
                        },
                    ));
                }
                _ => return Ok(McpAuthorization::Denied),
            }
        }

        let expires_at = now.saturating_add(CONFIRMATION_TTL_SECS);
        tx.execute(
            "INSERT INTO mcp_confirmation_requests
             (client_id, capability, account_id, target_id, fingerprint, status, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
            params![
                client_id,
                capability,
                account_id.map(AccountId::as_str),
                target_id,
                fingerprint,
                now,
                expires_at
            ],
        )
        .map_err(sql_err)?;
        let id = tx.last_insert_rowid();
        tx.commit().map_err(sql_err)?;
        Ok(McpAuthorization::NeedsConfirmation(
            McpConfirmationRequest {
                id,
                client_id: client_id.into(),
                capability: capability.into(),
                account_id: account_id.cloned(),
                target_id: target_id.map(str::to_owned),
                target_count: 1,
                expires_at,
            },
        ))
    }

    /// Lists only unexpired pending confirmations for the GTK window.  This
    /// Core query is intentionally bounded and carries no mail contents.
    pub fn list_pending_mcp_confirmations(
        &mut self,
        limit: usize,
    ) -> Result<Vec<McpConfirmationRequest>, CoreError> {
        let now = self.now();
        let conn = self.db.conn();
        if !mcp_enabled_in(conn)? {
            return Ok(Vec::new());
        }
        conn.execute(
            "UPDATE mcp_confirmation_requests
             SET status = 'expired', resolved_at = ?1
             WHERE status = 'pending' AND expires_at <= ?1",
            params![now],
        )
        .map_err(sql_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT id, client_id, capability, account_id, target_id, target_count, expires_at
                 FROM mcp_confirmation_requests
                 WHERE status = 'pending' AND expires_at > ?1
                 ORDER BY created_at ASC, id ASC LIMIT ?2",
            )
            .map_err(sql_err)?;
        let requests = stmt
            .query_map(params![now, limit.clamp(1, 20) as i64], |row| {
                Ok(McpConfirmationRequest {
                    id: row.get(0)?,
                    client_id: row.get(1)?,
                    capability: row.get(2)?,
                    account_id: row.get::<_, Option<String>>(3)?.map(AccountId),
                    target_id: row.get(4)?,
                    target_count: row.get(5)?,
                    expires_at: row.get(6)?,
                })
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(requests)
    }

    /// Resolves one pending GTK confirmation.  The state transition and an
    /// Always-allow grant share one SQLite transaction; stale/expired rows are
    /// an atomic no-op rather than an accidental approval.
    pub fn resolve_mcp_confirmation(
        &mut self,
        request_id: i64,
        choice: McpConfirmationChoice,
    ) -> Result<bool, CoreError> {
        let now = self.now();
        let conn = self.db.conn();
        let tx = conn.unchecked_transaction().map_err(sql_err)?;
        if !mcp_enabled_in(&tx)? {
            return Ok(false);
        }
        let row: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT client_id, capability, expires_at
                 FROM mcp_confirmation_requests
                 WHERE id = ?1 AND status = 'pending'",
                params![request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(sql_err)?;
        let Some((client_id, capability, expires_at)) = row else {
            return Ok(false);
        };
        if expires_at <= now {
            tx.execute(
                "UPDATE mcp_confirmation_requests
                 SET status = 'expired', resolved_at = ?2
                 WHERE id = ?1 AND status = 'pending'",
                params![request_id, now],
            )
            .map_err(sql_err)?;
            tx.commit().map_err(sql_err)?;
            return Ok(false);
        }
        if choice == McpConfirmationChoice::AlwaysAllow {
            tx.execute(
                "INSERT INTO mcp_permissions (client_id, capability, grant)
                 VALUES (?1, ?2, 'allow')
                 ON CONFLICT(client_id, capability) DO UPDATE SET grant = 'allow'",
                params![client_id, capability],
            )
            .map_err(sql_err)?;
        }
        let status = match choice {
            McpConfirmationChoice::Deny => "denied",
            McpConfirmationChoice::AllowOnce => "allow_once",
            McpConfirmationChoice::AlwaysAllow => "allowed_always",
        };
        let updated = tx
            .execute(
                "UPDATE mcp_confirmation_requests
                 SET status = ?2, resolved_at = ?3
                 WHERE id = ?1 AND status = 'pending' AND expires_at > ?3",
                params![request_id, status, now],
            )
            .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(updated == 1)
    }

    /// Lists a small, durable projection of the enrolled local MCP profiles.
    /// This is intentionally available even when the global MCP switch is
    /// off, so Settings can show that a client remains revoked rather than
    /// implying that turning the global switch back on restores access.
    pub fn list_mcp_clients(&self, limit: usize) -> Result<Vec<McpClientSummary>, CoreError> {
        let mut stmt = self
            .db
            .conn()
            .prepare(
                "SELECT id, name, enabled, permission_level
                 FROM mcp_clients
                 ORDER BY created_at ASC, id ASC
                 LIMIT ?1",
            )
            .map_err(sql_err)?;
        let clients = stmt
            .query_map(params![limit.clamp(1, 20) as i64], |row| {
                Ok(McpClientSummary {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    enabled: row.get::<_, i64>(2)? != 0,
                    permission_level: McpPermissionLevel::parse(&row.get::<_, String>(3)?),
                })
            })
            .map_err(sql_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(sql_err)?;
        Ok(clients)
    }

    /// Revokes one enrolled local MCP profile.  The transaction is the policy
    /// linearization point shared with authorization and GTK confirmation:
    /// pending and unconsumed one-time approvals become terminal, while every
    /// durable per-tool grant disappears before the disabled client is
    /// observable again.  This cannot cancel an action whose authorization
    /// transaction had already succeeded before the revoke began.
    pub fn revoke_mcp_client(&mut self, client_id: &str) -> Result<bool, CoreError> {
        if !valid_identifier(client_id, MAX_CLIENT_ID_BYTES) {
            return Ok(false);
        }

        let now = self.now();
        let tx = self.db.conn().unchecked_transaction().map_err(sql_err)?;
        let enabled: Option<i64> = tx
            .query_row(
                "SELECT enabled FROM mcp_clients WHERE id = ?1",
                params![client_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if enabled != Some(1) {
            return Ok(false);
        }

        tx.execute(
            "UPDATE mcp_clients SET enabled = 0 WHERE id = ?1 AND enabled = 1",
            params![client_id],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM mcp_permissions WHERE client_id = ?1",
            params![client_id],
        )
        .map_err(sql_err)?;
        tx.execute(
            "UPDATE mcp_confirmation_requests
             SET status = 'revoked', resolved_at = ?2
             WHERE client_id = ?1 AND status IN ('pending', 'allow_once')",
            params![client_id, now],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(true)
    }

    /// Explicitly starts a fresh default policy for the one revoked local
    /// `stdio` profile. This is deliberately narrower than provisioning: it
    /// neither creates a client nor re-enables one while the global MCP switch
    /// is off. A re-enabled profile always starts at Draft, with no durable
    /// grants; historical confirmation rows stay terminal and cannot revive.
    pub fn reenable_mcp_client(&mut self, client_id: &str) -> Result<bool, CoreError> {
        if client_id != "stdio" {
            return Ok(false);
        }

        let tx = self.db.conn().unchecked_transaction().map_err(sql_err)?;
        if !mcp_enabled_in(&tx)? {
            return Ok(false);
        }
        let enabled: Option<i64> = tx
            .query_row(
                "SELECT enabled FROM mcp_clients WHERE id = ?1",
                params![client_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        if enabled != Some(0) {
            return Ok(false);
        }

        tx.execute(
            "UPDATE mcp_clients
             SET enabled = 1, permission_level = 'draft'
             WHERE id = ?1 AND enabled = 0",
            params![client_id],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM mcp_permissions WHERE client_id = ?1",
            params![client_id],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(true)
    }

    /// Changes the persisted permission ceiling for the one enrolled local
    /// `stdio` profile. A new level is a policy boundary: all per-tool grants
    /// and unresolved approvals are removed in the same transaction, so a
    /// former Always allow cannot outlive a later restriction.
    ///
    /// This intentionally cannot enrol, re-enable, or alter another client.
    /// A revoked or unknown profile is a safe no-op; explicit re-enrolment is
    /// a separate future Settings/Core contract.
    pub fn set_mcp_client_permission_level(
        &mut self,
        client_id: &str,
        permission_level: McpPermissionLevel,
    ) -> Result<bool, CoreError> {
        if client_id != "stdio" {
            return Ok(false);
        }

        let now = self.now();
        let tx = self.db.conn().unchecked_transaction().map_err(sql_err)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT permission_level FROM mcp_clients
                 WHERE id = ?1 AND enabled = 1",
                params![client_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_err)?;
        let Some(current) = current else {
            return Ok(false);
        };
        if McpPermissionLevel::parse(&current) == permission_level {
            tx.commit().map_err(sql_err)?;
            return Ok(false);
        }

        tx.execute(
            "UPDATE mcp_clients SET permission_level = ?2
             WHERE id = ?1 AND enabled = 1",
            params![client_id, permission_level.as_str()],
        )
        .map_err(sql_err)?;
        tx.execute(
            "DELETE FROM mcp_permissions WHERE client_id = ?1",
            params![client_id],
        )
        .map_err(sql_err)?;
        tx.execute(
            "UPDATE mcp_confirmation_requests
             SET status = 'invalidated', resolved_at = ?2
             WHERE client_id = ?1 AND status IN ('pending', 'allow_once')",
            params![client_id, now],
        )
        .map_err(sql_err)?;
        tx.commit().map_err(sql_err)?;
        Ok(true)
    }

    /// Changes the global local-MCP switch and, on first use, provisions the
    /// only default profile that D57 grants: the user's own `stdio` client at
    /// Read + Draft.  A process-selected `CLIENT_ID` never enrolls itself;
    /// unknown ids remain denied until a future Settings client manager adds
    /// them explicitly.
    pub fn set_mcp_enabled(&mut self, now_ms: u64, enabled: bool) -> Result<(), CoreError> {
        self.patch_settings(now_ms, |settings| settings.mcp_enabled = enabled);
        self.flush_settings()?;
        if enabled {
            self.db
                .conn()
                .execute(
                    "INSERT INTO mcp_clients (id, name, enabled, permission_level, created_at)
                     VALUES ('stdio', 'Local stdio', 1, 'draft', ?1)
                     ON CONFLICT(id) DO NOTHING",
                    params![self.now()],
                )
                .map_err(sql_err)?;
        }
        Ok(())
    }

    pub(crate) fn ensure_default_mcp_client_if_enabled(&self) -> Result<(), CoreError> {
        if !mcp_enabled_in(self.db.conn())? {
            return Ok(());
        }
        self.db
            .conn()
            .execute(
                "INSERT INTO mcp_clients (id, name, enabled, permission_level, created_at)
                 VALUES ('stdio', 'Local stdio', 1, 'draft', ?1)
                 ON CONFLICT(id) DO NOTHING",
                params![self.now()],
            )
            .map_err(sql_err)?;
        Ok(())
    }
}

fn mcp_enabled_in(conn: &rusqlite::Connection) -> Result<bool, CoreError> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'mcp_enabled'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    Ok(matches!(value.as_deref(), Some("true" | "1")))
}

fn valid_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .chars()
            .all(|character| !character.is_control() && character != '\u{0}')
}

/// Everything one high-risk MCP door has to check before it is allowed to
/// act, and the record it leaves when it is not.
///
/// This is deliberately one function rather than one per door. The checks
/// are the interesting part of the permission model -- MCP enabled, the
/// caller's ceiling *and* the stored level both at `Full`, the account
/// real, an explicit `deny` grant winning over everything, a one-shot
/// approval matching this exact fingerprint and not yet expired -- and two
/// copies of them would eventually stop agreeing. What differs between
/// doors is only what happens on [`HighRiskDecision::Proceed`].
///
/// The transaction is the caller's: this function never commits. On
/// `Proceed` the caller does its own work in the same transaction and
/// commits, so the approval it just consumed and the effect it authorises
/// land together or not at all. On `Denied` the caller drops the
/// transaction, which also rolls back the expiry sweep -- harmless, and it
/// keeps a denied caller from leaving any trace it could measure.
struct HighRiskGateInput<'a> {
    client_id: &'a str,
    ceiling: McpPermissionLevel,
    capability: &'static str,
    account_id: &'a AccountId,
    target_id: Option<&'a str>,
    target_count: u32,
    fingerprint: &'a str,
}

enum HighRiskDecision {
    Proceed,
    NeedsConfirmation(McpConfirmationRequest),
    Denied,
}

fn high_risk_gate(
    tx: &rusqlite::Transaction<'_>,
    input: HighRiskGateInput<'_>,
    now: i64,
) -> Result<HighRiskDecision, CoreError> {
    let HighRiskGateInput {
        client_id,
        ceiling,
        capability,
        account_id,
        target_id,
        target_count,
        fingerprint,
    } = input;
    if !mcp_enabled_in(tx)? || ceiling < McpPermissionLevel::Full {
        return Ok(HighRiskDecision::Denied);
    }
    let client: Option<(i64, String)> = tx
        .query_row(
            "SELECT enabled, permission_level FROM mcp_clients WHERE id = ?1",
            params![client_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_err)?;
    let Some((enabled, stored_level)) = client else {
        return Ok(HighRiskDecision::Denied);
    };
    if enabled != 1 || McpPermissionLevel::parse(&stored_level) < McpPermissionLevel::Full {
        return Ok(HighRiskDecision::Denied);
    }
    let account_exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM accounts WHERE id = ?1",
            params![account_id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    if account_exists.is_none() {
        return Err(CoreError::from_code(ErrorCode::AccountNotFound));
    }

    let grant: Option<String> = tx
        .query_row(
            "SELECT grant FROM mcp_permissions
             WHERE client_id = ?1 AND capability = ?2",
            params![client_id, capability],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_err)?;
    match grant.as_deref() {
        Some("deny") => return Ok(HighRiskDecision::Denied),
        Some("allow") => return Ok(HighRiskDecision::Proceed),
        _ => {}
    }

    tx.execute(
        "UPDATE mcp_confirmation_requests
         SET status = 'expired', resolved_at = ?1
         WHERE status = 'pending' AND expires_at <= ?1",
        params![now],
    )
    .map_err(sql_err)?;
    let existing: Option<(i64, String, i64)> = tx
        .query_row(
            "SELECT id, status, expires_at FROM mcp_confirmation_requests
             WHERE client_id = ?1 AND capability = ?2 AND fingerprint = ?3
               AND status IN ('pending', 'allow_once', 'denied')
             ORDER BY id DESC LIMIT 1",
            params![client_id, capability, fingerprint],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql_err)?;
    if let Some((id, status, expires_at)) = existing {
        return match status.as_str() {
            "allow_once" if expires_at > now => {
                let consumed = tx
                    .execute(
                        "UPDATE mcp_confirmation_requests
                         SET status = 'consumed', resolved_at = ?2
                         WHERE id = ?1 AND status = 'allow_once' AND expires_at > ?2",
                        params![id, now],
                    )
                    .map_err(sql_err)?;
                if consumed != 1 {
                    return Ok(HighRiskDecision::Denied);
                }
                Ok(HighRiskDecision::Proceed)
            }
            "pending" if expires_at > now => Ok(HighRiskDecision::NeedsConfirmation(
                McpConfirmationRequest {
                    id,
                    client_id: client_id.into(),
                    capability: capability.into(),
                    account_id: Some(account_id.clone()),
                    target_id: target_id.map(str::to_string),
                    target_count,
                    expires_at,
                },
            )),
            _ => Ok(HighRiskDecision::Denied),
        };
    }

    let expires_at = now.saturating_add(CONFIRMATION_TTL_SECS);
    tx.execute(
        "INSERT INTO mcp_confirmation_requests
         (client_id, capability, account_id, target_id, target_count, fingerprint, status, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
        params![
            client_id,
            capability,
            account_id.as_str(),
            target_id,
            i64::from(target_count),
            fingerprint,
            now,
            expires_at,
        ],
    )
    .map_err(sql_err)?;
    let id = tx.last_insert_rowid();
    Ok(HighRiskDecision::NeedsConfirmation(
        McpConfirmationRequest {
            id,
            client_id: client_id.into(),
            capability: capability.into(),
            account_id: Some(account_id.clone()),
            target_id: target_id.map(str::to_string),
            target_count,
            expires_at,
        },
    ))
}

/// T-060u: the same binding as [`bulk_high_risk_fingerprint`], for a door
/// whose target is a single folder rather than a set of threads. Separate
/// domain separator, so no approval can ever cross between the two shapes
/// even if a folder id and a thread id were ever to collide.
fn folder_high_risk_fingerprint(
    capability: &'static str,
    account_id: &AccountId,
    folder_id: &FolderId,
) -> Result<String, CoreError> {
    if !valid_identifier(account_id.as_str(), MAX_TARGET_ID_BYTES)
        || !valid_identifier(folder_id.as_str(), MAX_TARGET_ID_BYTES)
    {
        return Err(CoreError::from_code(ErrorCode::InvalidArgument));
    }
    let domain = match capability {
        "delete_folder" => b"feathermail:mcp:delete_folder:v1".as_slice(),
        _ => return Err(CoreError::from_code(ErrorCode::InvalidArgument)),
    };
    let mut digest = Sha256::new();
    digest.update(domain);
    hash_fingerprint_part(&mut digest, account_id.as_str());
    hash_fingerprint_part(&mut digest, folder_id.as_str());
    Ok(format!("{capability}:v1:{:x}", digest.finalize()))
}

/// Stable, order-independent binding for one high-risk batch approval. Values
/// are length-framed before hashing, so no separator ambiguity can turn two
/// different id lists into one approval. The capability's domain separator is
/// part of the digest, so a Trash grant cannot approve a permanent deletion.
/// The digest is deliberately distinct from operation payload hashes, which
/// are not authorization primitives.
fn bulk_high_risk_fingerprint(
    capability: &'static str,
    account_id: &AccountId,
    thread_ids: &[ThreadId],
) -> Result<(String, u32), CoreError> {
    if !valid_identifier(account_id.as_str(), MAX_TARGET_ID_BYTES)
        || thread_ids.is_empty()
        || thread_ids.len() > MAX_MCP_HIGH_RISK_BATCH_TARGETS
    {
        return Err(CoreError::from_code(ErrorCode::InvalidArgument));
    }
    let mut ids = thread_ids.iter().collect::<Vec<_>>();
    if ids
        .iter()
        .any(|id| !valid_identifier(id.as_str(), MAX_TARGET_ID_BYTES))
    {
        return Err(CoreError::from_code(ErrorCode::InvalidArgument));
    }
    ids.sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
    if ids
        .windows(2)
        .any(|pair| pair[0].as_str() == pair[1].as_str())
    {
        return Err(CoreError::from_code(ErrorCode::InvalidArgument));
    }

    let mut digest = Sha256::new();
    let domain = match capability {
        "bulk_delete" => b"feathermail:mcp:bulk_delete:v1".as_slice(),
        "bulk_permanent_delete" => b"feathermail:mcp:bulk_permanent_delete:v1".as_slice(),
        _ => return Err(CoreError::from_code(ErrorCode::InvalidArgument)),
    };
    digest.update(domain);
    hash_fingerprint_part(&mut digest, account_id.as_str());
    digest.update((ids.len() as u64).to_be_bytes());
    for id in ids {
        hash_fingerprint_part(&mut digest, id.as_str());
    }
    Ok((
        format!("{capability}:v1:{:x}", digest.finalize()),
        thread_ids.len() as u32,
    ))
}

fn hash_fingerprint_part(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DraftContent;

    fn enabled_core() -> Core {
        let mut core = Core::memory().unwrap();
        core.set_now(1_000);
        core.set_mcp_enabled(1_000, true).unwrap();
        core
    }

    fn auth(
        core: &mut Core,
        ceiling: McpPermissionLevel,
        capability: &str,
        level: McpPermissionLevel,
        confirm: bool,
        fingerprint: &str,
    ) -> McpAuthorization {
        core.authorize_mcp_action(
            "stdio",
            ceiling,
            capability,
            level,
            confirm,
            None,
            confirm.then_some("test-target"),
            fingerprint,
        )
        .unwrap()
    }

    fn draft(core: &Core) -> (AccountId, DraftId) {
        let account_id = AccountId("john".into());
        core.db
            .conn()
            .execute(
                "INSERT INTO accounts (id, name, email, provider, status, download_policy, created_at, updated_at)
                 VALUES ('john', 'John', 'john@example.test', 'generic', 'synced', 'recent', 0, 0)",
                [],
            )
            .unwrap();
        let draft = core
            .save_draft(
                &account_id,
                None,
                DraftContent {
                    from: "john@example.test".into(),
                    to: "recipient@example.test".into(),
                    subject: "subject".into(),
                    body: "body never enters confirmation storage".into(),
                    ..DraftContent::default()
                },
            )
            .unwrap();
        (account_id, draft.id)
    }

    fn set_stdio_level(core: &mut Core, permission_level: McpPermissionLevel) {
        assert!(core
            .set_mcp_client_permission_level("stdio", permission_level)
            .unwrap());
    }

    fn custom_folder(core: &Core, id: &str, name: &str) {
        core.db
            .conn()
            .execute(
                "INSERT INTO folders (id, account_id, remote_id, name, kind, delimiter)
                 VALUES (?1, 'john', ?2, ?2, 'custom', '/')",
                params![id, name],
            )
            .unwrap();
    }

    fn folder_deleted_at(core: &Core, id: &str) -> Option<i64> {
        core.db
            .conn()
            .query_row(
                "SELECT deleted_at FROM folders WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// T-060u: a folder deletion is irreversible, so it sits at the same
    /// height as a bulk permanent delete -- `Full` ceiling, `Full` stored
    /// level, and a GTK approval bound to this exact folder.
    #[test]
    fn delete_folder_door_needs_full_on_both_sides_and_a_matching_approval() {
        let mut core = enabled_core();
        let (account_id, _) = draft(&core);
        custom_folder(&core, "john:ideas", "Ideas");
        let folder = FolderId("john:ideas".into());

        // Stored level still Draft: nothing may reach a confirmation.
        assert_eq!(
            core.queue_mcp_delete_folder("stdio", McpPermissionLevel::Full, &account_id, &folder)
                .unwrap(),
            McpFolderDeleteOutcome::Denied
        );
        set_stdio_level(&mut core, McpPermissionLevel::Full);
        // Caller's own ceiling is what narrows a stored policy, never widens it.
        assert_eq!(
            core.queue_mcp_delete_folder("stdio", McpPermissionLevel::Send, &account_id, &folder)
                .unwrap(),
            McpFolderDeleteOutcome::Denied
        );

        let McpFolderDeleteOutcome::NeedsConfirmation(request) = core
            .queue_mcp_delete_folder("stdio", McpPermissionLevel::Full, &account_id, &folder)
            .unwrap()
        else {
            panic!("deleting a folder needs GTK approval");
        };
        assert_eq!(request.capability, "delete_folder");
        assert_eq!(request.target_id.as_deref(), Some("john:ideas"));
        assert_eq!(request.target_count, 1);
        assert!(
            folder_deleted_at(&core, "john:ideas").is_none(),
            "asking is not doing"
        );

        assert!(core
            .resolve_mcp_confirmation(request.id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        assert_eq!(
            core.queue_mcp_delete_folder("stdio", McpPermissionLevel::Full, &account_id, &folder)
                .unwrap(),
            McpFolderDeleteOutcome::Deleted { queued: true }
        );
        assert!(folder_deleted_at(&core, "john:ideas").is_some());
    }

    /// The approval is bound to one folder. An agent that gets permission
    /// to delete `Ideas` must not be able to spend it on `Plans` -- the
    /// same rule the bulk doors enforce over a thread set.
    #[test]
    fn a_delete_folder_approval_cannot_be_spent_on_a_different_folder() {
        let mut core = enabled_core();
        let (account_id, _) = draft(&core);
        set_stdio_level(&mut core, McpPermissionLevel::Full);
        custom_folder(&core, "john:ideas", "Ideas");
        custom_folder(&core, "john:plans", "Plans");

        let McpFolderDeleteOutcome::NeedsConfirmation(request) = core
            .queue_mcp_delete_folder(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                &FolderId("john:ideas".into()),
            )
            .unwrap()
        else {
            panic!("expected a confirmation request");
        };
        let fingerprint: String = core
            .db
            .conn()
            .query_row(
                "SELECT fingerprint FROM mcp_confirmation_requests WHERE id = ?1",
                params![request.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(fingerprint.starts_with("delete_folder:v1:"));
        assert!(
            !fingerprint.contains("john:ideas"),
            "the digest binds the folder, it does not store it"
        );
        assert!(core
            .resolve_mcp_confirmation(request.id, McpConfirmationChoice::AllowOnce)
            .unwrap());

        let other = core
            .queue_mcp_delete_folder(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                &FolderId("john:plans".into()),
            )
            .unwrap();
        assert!(
            matches!(other, McpFolderDeleteOutcome::NeedsConfirmation(_)),
            "a different folder needs its own approval, got {other:?}"
        );
        assert!(folder_deleted_at(&core, "john:plans").is_none());
    }

    /// Holding a valid approval does not exempt an agent from the rules a
    /// user is held to. The folder filled up between the ask and the spend;
    /// the deletion is refused, and -- because the refusal aborts the same
    /// transaction that consumed the approval -- the approval is not burned
    /// either, so retrying after moving the mail still works.
    #[test]
    fn an_approved_delete_of_a_folder_that_filled_up_is_still_refused() {
        let mut core = enabled_core();
        let (account_id, _) = draft(&core);
        set_stdio_level(&mut core, McpPermissionLevel::Full);
        custom_folder(&core, "john:ideas", "Ideas");
        let folder = FolderId("john:ideas".into());

        let McpFolderDeleteOutcome::NeedsConfirmation(request) = core
            .queue_mcp_delete_folder("stdio", McpPermissionLevel::Full, &account_id, &folder)
            .unwrap()
        else {
            panic!("expected a confirmation request");
        };
        assert!(core
            .resolve_mcp_confirmation(request.id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        core.db
            .conn()
            .execute(
                "INSERT INTO threads (id, account_id, folder_id, subject, snippet, date, unread)
                 VALUES ('t-late', 'john', 'john:ideas', 'Late', 'Late', 900, 0)",
                [],
            )
            .unwrap();

        let err = core
            .queue_mcp_delete_folder("stdio", McpPermissionLevel::Full, &account_id, &folder)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(folder_deleted_at(&core, "john:ideas").is_none());

        core.db
            .conn()
            .execute("DELETE FROM threads WHERE id = 't-late'", [])
            .unwrap();
        assert_eq!(
            core.queue_mcp_delete_folder("stdio", McpPermissionLevel::Full, &account_id, &folder)
                .unwrap(),
            McpFolderDeleteOutcome::Deleted { queued: true },
            "the approval survived a refusal it never caused"
        );
    }

    #[test]
    fn bulk_high_risk_confirmation_keeps_only_a_safe_count_and_capability_bound_digest() {
        let mut core = enabled_core();
        set_stdio_level(&mut core, McpPermissionLevel::Full);
        let (account_id, _) = draft(&core);
        let first_ids = vec![
            ThreadId("opaque-bulk-target-b".into()),
            ThreadId("opaque-bulk-target-a".into()),
        ];
        let McpBulkHighRiskOutcome::NeedsConfirmation(first) = core
            .queue_mcp_bulk_trash(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                first_ids.clone(),
            )
            .unwrap()
        else {
            panic!("full bulk Trash needs GTK approval");
        };
        assert_eq!(first.target_id, None);
        assert_eq!(first.target_count, 2);
        let (stored_target, fingerprint, stored_count): (Option<String>, String, i64) = core
            .db
            .conn()
            .query_row(
                "SELECT target_id, fingerprint, target_count
                 FROM mcp_confirmation_requests WHERE id = ?1",
                params![first.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored_target, None);
        assert_eq!(stored_count, 2);
        assert!(fingerprint.starts_with("bulk_delete:v1:"));
        for raw_id in ["opaque-bulk-target-a", "opaque-bulk-target-b"] {
            assert!(!fingerprint.contains(raw_id));
        }

        let McpBulkHighRiskOutcome::NeedsConfirmation(reordered) = core
            .queue_mcp_bulk_trash(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                first_ids.into_iter().rev().collect(),
            )
            .unwrap()
        else {
            panic!("same set must reuse its pending request");
        };
        assert_eq!(reordered.id, first.id);
        let McpBulkHighRiskOutcome::NeedsConfirmation(changed) = core
            .queue_mcp_bulk_trash(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                vec![
                    ThreadId("opaque-bulk-target-a".into()),
                    ThreadId("opaque-bulk-target-c".into()),
                ],
            )
            .unwrap()
        else {
            panic!("changed set needs a new approval");
        };
        assert_ne!(changed.id, first.id);

        let McpBulkHighRiskOutcome::NeedsConfirmation(permanent) = core
            .queue_mcp_bulk_permanent_delete(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                vec![
                    ThreadId("opaque-bulk-target-a".into()),
                    ThreadId("opaque-bulk-target-b".into()),
                ],
            )
            .unwrap()
        else {
            panic!("full bulk permanent delete needs GTK approval");
        };
        assert_eq!(permanent.capability, "bulk_permanent_delete");
        assert_eq!(permanent.target_id, None);
        assert_eq!(permanent.target_count, 2);
        let permanent_fingerprint: String = core
            .db
            .conn()
            .query_row(
                "SELECT fingerprint FROM mcp_confirmation_requests WHERE id = ?1",
                params![permanent.id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(permanent_fingerprint.starts_with("bulk_permanent_delete:v1:"));
        assert_ne!(permanent_fingerprint, fingerprint);
    }

    #[test]
    fn bulk_trash_core_boundary_rejects_duplicate_oversized_and_control_ids() {
        let mut core = enabled_core();
        let account_id = AccountId("account".into());
        let duplicate = core
            .queue_mcp_bulk_trash(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                vec![ThreadId("same".into()), ThreadId("same".into())],
            )
            .unwrap_err();
        assert_eq!(duplicate.code, ErrorCode::InvalidArgument);
        let control = core
            .queue_mcp_bulk_trash(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                vec![ThreadId("bad\nidentifier".into())],
            )
            .unwrap_err();
        assert_eq!(control.code, ErrorCode::InvalidArgument);
        let oversized = core
            .queue_mcp_bulk_trash(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                (0..=MAX_MCP_HIGH_RISK_BATCH_TARGETS)
                    .map(|number| ThreadId(format!("target-{number}")))
                    .collect(),
            )
            .unwrap_err();
        assert_eq!(oversized.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn default_draft_policy_allows_low_risk_but_env_full_cannot_grant_send() {
        let mut core = enabled_core();
        assert_eq!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "archive_message",
                McpPermissionLevel::Draft,
                false,
                "archive:v1",
            ),
            McpAuthorization::Allowed
        );
        assert_eq!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "send_draft",
                McpPermissionLevel::Send,
                true,
                "draft:one:7",
            ),
            McpAuthorization::Denied
        );
    }

    #[test]
    fn specialised_send_and_bulk_doors_obey_the_persisted_high_risk_ceiling() {
        let mut core = enabled_core();
        let (account_id, draft_id) = draft(&core);
        assert_eq!(
            core.queue_mcp_draft_send("stdio", McpPermissionLevel::Send, &account_id, &draft_id,)
                .unwrap(),
            McpSendOutcome::Denied
        );
        assert_eq!(
            core.queue_mcp_bulk_trash(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                vec![ThreadId("opaque-thread".into())],
            )
            .unwrap(),
            McpBulkHighRiskOutcome::Denied
        );

        set_stdio_level(&mut core, McpPermissionLevel::Send);
        assert!(matches!(
            core.queue_mcp_draft_send("stdio", McpPermissionLevel::Send, &account_id, &draft_id,)
                .unwrap(),
            McpSendOutcome::NeedsConfirmation(_)
        ));
        assert_eq!(
            core.queue_mcp_bulk_trash(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                vec![ThreadId("opaque-thread".into())],
            )
            .unwrap(),
            McpBulkHighRiskOutcome::Denied
        );

        set_stdio_level(&mut core, McpPermissionLevel::Full);
        assert!(matches!(
            core.queue_mcp_bulk_permanent_delete(
                "stdio",
                McpPermissionLevel::Full,
                &account_id,
                vec![ThreadId("opaque-thread".into())],
            )
            .unwrap(),
            McpBulkHighRiskOutcome::NeedsConfirmation(_)
        ));
    }

    #[test]
    fn unknown_client_id_never_enrolls_itself() {
        let mut core = enabled_core();
        assert_eq!(
            core.authorize_mcp_action(
                "forged-client",
                McpPermissionLevel::Full,
                "archive_message",
                McpPermissionLevel::Draft,
                false,
                None,
                None,
                "archive:v1",
            )
            .unwrap(),
            McpAuthorization::Denied
        );
    }

    #[test]
    fn client_list_is_bounded_to_durable_policy_metadata() {
        let mut core = enabled_core();
        for number in 0..24 {
            core.db
                .conn()
                .execute(
                    "INSERT INTO mcp_clients (id, name, enabled, permission_level, created_at)
                     VALUES (?1, ?2, 1, 'read', ?3)",
                    params![
                        format!("local-{number:02}"),
                        format!("Local profile {number:02}"),
                        2_000 + number
                    ],
                )
                .unwrap();
        }

        let one = core.list_mcp_clients(0).unwrap();
        assert_eq!(one.len(), 1, "the settings query is never unbounded");
        assert_eq!(one[0].id, "stdio");
        assert_eq!(one[0].name, "Local stdio");
        assert!(one[0].enabled);
        assert_eq!(one[0].permission_level, McpPermissionLevel::Draft);
        assert_eq!(core.list_mcp_clients(usize::MAX).unwrap().len(), 20);
        assert!(!core.revoke_mcp_client("unknown-client").unwrap());
        assert_eq!(
            core.list_mcp_clients(usize::MAX).unwrap().len(),
            20,
            "revoking an unknown id must not create or remove a profile"
        );
        assert!(
            !core
                .set_mcp_client_permission_level("local-00", McpPermissionLevel::Full)
                .unwrap(),
            "only the enrolled local stdio profile has a Settings permission door"
        );
    }

    #[test]
    fn changing_stdio_permission_is_an_atomic_ceiling_and_clears_old_approvals() {
        let mut core = enabled_core();
        set_stdio_level(&mut core, McpPermissionLevel::Full);

        let McpAuthorization::NeedsConfirmation(always) = auth(
            &mut core,
            McpPermissionLevel::Send,
            "send_draft",
            McpPermissionLevel::Send,
            true,
            "draft:always:1",
        ) else {
            panic!("Send needs confirmation at the Full policy level");
        };
        assert!(core
            .resolve_mcp_confirmation(always.id, McpConfirmationChoice::AlwaysAllow)
            .unwrap());
        let McpAuthorization::NeedsConfirmation(once) = auth(
            &mut core,
            McpPermissionLevel::Full,
            "delete_message",
            McpPermissionLevel::Full,
            true,
            "thread:once",
        ) else {
            panic!("Delete needs confirmation at the Full policy level");
        };
        assert!(core
            .resolve_mcp_confirmation(once.id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        let McpAuthorization::NeedsConfirmation(pending) = auth(
            &mut core,
            McpPermissionLevel::Full,
            "permanent_delete",
            McpPermissionLevel::Full,
            true,
            "thread:pending",
        ) else {
            panic!("Permanent delete needs confirmation at the Full policy level");
        };
        core.db
            .conn()
            .execute(
                "INSERT INTO mcp_permissions (client_id, capability, grant)
                 VALUES ('stdio', 'create_folder', 'deny')",
                [],
            )
            .unwrap();

        set_stdio_level(&mut core, McpPermissionLevel::Read);
        assert!(
            !core
                .set_mcp_client_permission_level("stdio", McpPermissionLevel::Read)
                .unwrap(),
            "reselecting the current level is not a destructive policy reset"
        );
        assert_eq!(
            core.list_mcp_clients(20).unwrap()[0].permission_level,
            McpPermissionLevel::Read
        );
        let grants: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM mcp_permissions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(grants, 0, "a policy change removes every durable grant");
        let statuses: Vec<String> = core
            .db
            .conn()
            .prepare(
                "SELECT status FROM mcp_confirmation_requests
                 WHERE id IN (?1, ?2) ORDER BY id",
            )
            .unwrap()
            .query_map(params![once.id, pending.id], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(statuses, vec!["invalidated", "invalidated"]);
        assert!(core.list_pending_mcp_confirmations(20).unwrap().is_empty());
        for (capability, required, confirmation, fingerprint) in [
            (
                "archive_message",
                McpPermissionLevel::Draft,
                false,
                "archive:after-read",
            ),
            (
                "send_draft",
                McpPermissionLevel::Send,
                true,
                "draft:after-read:1",
            ),
            (
                "delete_message",
                McpPermissionLevel::Full,
                true,
                "thread:after-read",
            ),
        ] {
            assert_eq!(
                auth(
                    &mut core,
                    McpPermissionLevel::Full,
                    capability,
                    required,
                    confirmation,
                    fingerprint,
                ),
                McpAuthorization::Denied,
                "a Full environment cannot raise persisted Read for {capability}"
            );
        }

        set_stdio_level(&mut core, McpPermissionLevel::Send);
        assert!(matches!(
            auth(
                &mut core,
                McpPermissionLevel::Send,
                "send_draft",
                McpPermissionLevel::Send,
                true,
                "draft:at-send:1",
            ),
            McpAuthorization::NeedsConfirmation(_)
        ));
        assert_eq!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "delete_message",
                McpPermissionLevel::Full,
                true,
                "thread:at-send",
            ),
            McpAuthorization::Denied
        );
        set_stdio_level(&mut core, McpPermissionLevel::Full);
        assert!(matches!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "delete_message",
                McpPermissionLevel::Full,
                true,
                "thread:at-full",
            ),
            McpAuthorization::NeedsConfirmation(_)
        ));
    }

    #[test]
    fn changed_stdio_permission_persists_across_restart_without_enrolment_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let mut core = Core::open(&path).unwrap();
            core.set_now(1_000);
            core.set_mcp_enabled(1_000, true).unwrap();
            set_stdio_level(&mut core, McpPermissionLevel::Send);
        }

        let reopened = Core::open(&path).unwrap();
        let clients = reopened.list_mcp_clients(20).unwrap();
        assert_eq!(clients.len(), 1);
        assert!(clients[0].enabled);
        assert_eq!(clients[0].permission_level, McpPermissionLevel::Send);
    }

    #[test]
    fn revoking_client_invalidates_pending_once_and_durable_grants() {
        let mut core = enabled_core();
        set_stdio_level(&mut core, McpPermissionLevel::Full);
        let McpAuthorization::NeedsConfirmation(always) = auth(
            &mut core,
            McpPermissionLevel::Send,
            "send_draft",
            McpPermissionLevel::Send,
            true,
            "draft:always:1",
        ) else {
            panic!("send needs confirmation");
        };
        assert!(core
            .resolve_mcp_confirmation(always.id, McpConfirmationChoice::AlwaysAllow)
            .unwrap());
        let McpAuthorization::NeedsConfirmation(once) = auth(
            &mut core,
            McpPermissionLevel::Full,
            "delete_message",
            McpPermissionLevel::Full,
            true,
            "thread:once",
        ) else {
            panic!("delete needs confirmation");
        };
        assert!(core
            .resolve_mcp_confirmation(once.id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        let McpAuthorization::NeedsConfirmation(pending) = auth(
            &mut core,
            McpPermissionLevel::Full,
            "permanent_delete",
            McpPermissionLevel::Full,
            true,
            "thread:pending",
        ) else {
            panic!("permanent delete needs confirmation");
        };
        core.db
            .conn()
            .execute(
                "INSERT INTO mcp_permissions (client_id, capability, grant)
                 VALUES ('stdio', 'create_folder', 'deny')",
                [],
            )
            .unwrap();

        // A resolution that commits before revoke may briefly create a grant;
        // the following revoke transaction is the later policy boundary and
        // removes it. The still-pending request then loses its race and
        // cannot be answered afterwards.
        assert!(core.revoke_mcp_client("stdio").unwrap());
        assert!(!core
            .resolve_mcp_confirmation(pending.id, McpConfirmationChoice::AlwaysAllow)
            .unwrap());
        assert!(!core.revoke_mcp_client("stdio").unwrap());

        let clients = core.list_mcp_clients(20).unwrap();
        assert_eq!(clients.len(), 1);
        assert!(!clients[0].enabled);
        let grants: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM mcp_permissions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(grants, 0, "revoke removes every per-tool policy row");
        let request_statuses: Vec<String> = core
            .db
            .conn()
            .prepare(
                "SELECT status FROM mcp_confirmation_requests
                 WHERE id IN (?1, ?2) ORDER BY id",
            )
            .unwrap()
            .query_map(params![once.id, pending.id], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(request_statuses, vec!["revoked", "revoked"]);
        assert!(core.list_pending_mcp_confirmations(20).unwrap().is_empty());
        assert_eq!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "archive_message",
                McpPermissionLevel::Draft,
                false,
                "archive:after-revoke",
            ),
            McpAuthorization::Denied
        );
        assert_eq!(
            core.queue_mcp_draft_send(
                "stdio",
                McpPermissionLevel::Send,
                &AccountId("account".into()),
                &DraftId("draft".into()),
            )
            .unwrap(),
            McpSendOutcome::Denied
        );
    }

    #[test]
    fn revoked_stdio_is_not_reenabled_by_global_switch_or_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        {
            let mut core = Core::open(&path).unwrap();
            core.set_now(1_000);
            core.set_mcp_enabled(1_000, true).unwrap();
            assert!(core.list_mcp_clients(20).unwrap()[0].enabled);
            assert!(core.revoke_mcp_client("stdio").unwrap());
            assert!(
                !core
                    .set_mcp_client_permission_level("stdio", McpPermissionLevel::Full)
                    .unwrap(),
                "changing a level must not re-enable a revoked profile"
            );
            core.set_mcp_enabled(1_001, false).unwrap();
            core.set_mcp_enabled(1_002, true).unwrap();
            assert!(
                !core.list_mcp_clients(20).unwrap()[0].enabled,
                "global enable must not silently restore a revoked profile"
            );
        }

        let mut reopened = Core::open(&path).unwrap();
        assert!(!reopened.list_mcp_clients(20).unwrap()[0].enabled);
        assert_eq!(
            auth(
                &mut reopened,
                McpPermissionLevel::Full,
                "archive_message",
                McpPermissionLevel::Draft,
                false,
                "archive:after-restart",
            ),
            McpAuthorization::Denied
        );
    }

    #[test]
    fn explicit_reenable_restores_only_a_fresh_default_stdio_policy() {
        let mut core = enabled_core();
        set_stdio_level(&mut core, McpPermissionLevel::Full);
        let McpAuthorization::NeedsConfirmation(always) = auth(
            &mut core,
            McpPermissionLevel::Send,
            "send_draft",
            McpPermissionLevel::Send,
            true,
            "draft:before-reenable:always",
        ) else {
            panic!("full profile needs a send confirmation");
        };
        assert!(core
            .resolve_mcp_confirmation(always.id, McpConfirmationChoice::AlwaysAllow)
            .unwrap());
        let McpAuthorization::NeedsConfirmation(pending) = auth(
            &mut core,
            McpPermissionLevel::Full,
            "delete_message",
            McpPermissionLevel::Full,
            true,
            "thread:before-reenable:pending",
        ) else {
            panic!("full profile needs a delete confirmation");
        };
        assert!(core.revoke_mcp_client("stdio").unwrap());

        core.set_mcp_enabled(1_001, false).unwrap();
        assert!(
            !core.reenable_mcp_client("stdio").unwrap(),
            "the global switch is a live prerequisite"
        );
        core.set_mcp_enabled(1_002, true).unwrap();
        assert!(
            !core.list_mcp_clients(20).unwrap()[0].enabled,
            "an off/on cycle must not replace explicit re-enable"
        );
        assert!(!core.reenable_mcp_client("other").unwrap());
        assert!(core.reenable_mcp_client("stdio").unwrap());
        assert!(!core.reenable_mcp_client("stdio").unwrap());

        assert_eq!(
            core.list_mcp_clients(20).unwrap(),
            vec![McpClientSummary {
                id: "stdio".into(),
                name: "Local stdio".into(),
                enabled: true,
                permission_level: McpPermissionLevel::Draft,
            }]
        );
        let grants: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM mcp_permissions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(grants, 0, "a fresh profile cannot retain old Always grants");
        let request_statuses: Vec<String> = core
            .db
            .conn()
            .prepare(
                "SELECT status FROM mcp_confirmation_requests
                 WHERE id IN (?1, ?2) ORDER BY id",
            )
            .unwrap()
            .query_map(params![always.id, pending.id], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(request_statuses, vec!["allowed_always", "revoked"]);
        assert!(
            !core
                .resolve_mcp_confirmation(pending.id, McpConfirmationChoice::AllowOnce)
                .unwrap(),
            "a terminal pre-revoke request can never dispatch after re-enable"
        );

        assert_eq!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "archive_message",
                McpPermissionLevel::Draft,
                false,
                "archive:after-reenable",
            ),
            McpAuthorization::Allowed,
            "Draft low-risk work is available again"
        );
        assert_eq!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "send_draft",
                McpPermissionLevel::Send,
                true,
                "draft:after-reenable:blocked",
            ),
            McpAuthorization::Denied,
            "a full process ceiling cannot raise fresh Draft policy"
        );
        set_stdio_level(&mut core, McpPermissionLevel::Send);
        let McpAuthorization::NeedsConfirmation(fresh) = auth(
            &mut core,
            McpPermissionLevel::Full,
            "send_draft",
            McpPermissionLevel::Send,
            true,
            "draft:after-reenable:fresh",
        ) else {
            panic!("raising the fresh profile to Send must require a new GTK decision");
        };
        assert_ne!(fresh.id, pending.id);
    }

    #[test]
    fn one_time_approval_is_consumed_and_always_is_per_action() {
        let mut core = enabled_core();
        set_stdio_level(&mut core, McpPermissionLevel::Full);
        let McpAuthorization::NeedsConfirmation(request) = auth(
            &mut core,
            McpPermissionLevel::Send,
            "send_draft",
            McpPermissionLevel::Send,
            true,
            "draft:one:7",
        ) else {
            panic!("send needs confirmation");
        };
        assert!(core
            .resolve_mcp_confirmation(request.id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        assert_eq!(
            auth(
                &mut core,
                McpPermissionLevel::Send,
                "send_draft",
                McpPermissionLevel::Send,
                true,
                "draft:one:7",
            ),
            McpAuthorization::Allowed
        );
        assert!(matches!(
            auth(
                &mut core,
                McpPermissionLevel::Send,
                "send_draft",
                McpPermissionLevel::Send,
                true,
                "draft:one:7",
            ),
            McpAuthorization::NeedsConfirmation(_)
        ));

        let McpAuthorization::NeedsConfirmation(request) = auth(
            &mut core,
            McpPermissionLevel::Full,
            "delete_message",
            McpPermissionLevel::Full,
            true,
            "thread:one",
        ) else {
            panic!("delete needs confirmation");
        };
        assert!(core
            .resolve_mcp_confirmation(request.id, McpConfirmationChoice::AlwaysAllow)
            .unwrap());
        assert_eq!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "delete_message",
                McpPermissionLevel::Full,
                true,
                "thread:two",
            ),
            McpAuthorization::Allowed
        );
        assert!(matches!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "send_draft",
                McpPermissionLevel::Send,
                true,
                "draft:two:1",
            ),
            McpAuthorization::NeedsConfirmation(_)
        ));
    }

    #[test]
    fn stale_or_disabled_confirmations_are_noops() {
        let mut core = enabled_core();
        set_stdio_level(&mut core, McpPermissionLevel::Send);
        let McpAuthorization::NeedsConfirmation(request) = auth(
            &mut core,
            McpPermissionLevel::Send,
            "send_draft",
            McpPermissionLevel::Send,
            true,
            "draft:one:7",
        ) else {
            panic!("send needs confirmation");
        };
        core.set_now(1_121);
        assert!(!core
            .resolve_mcp_confirmation(request.id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        core.patch_settings(1_121, |settings| settings.mcp_enabled = false);
        core.flush_settings().unwrap();
        assert_eq!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "archive_message",
                McpPermissionLevel::Draft,
                false,
                "archive:v1",
            ),
            McpAuthorization::Denied
        );
    }

    #[test]
    fn deny_decision_is_terminal_for_that_request_and_never_creates_a_grant() {
        let mut core = enabled_core();
        set_stdio_level(&mut core, McpPermissionLevel::Full);
        let McpAuthorization::NeedsConfirmation(request) = auth(
            &mut core,
            McpPermissionLevel::Full,
            "delete_message",
            McpPermissionLevel::Full,
            true,
            "delete:john:thread:1",
        ) else {
            panic!("delete needs GTK approval");
        };
        assert!(core
            .resolve_mcp_confirmation(request.id, McpConfirmationChoice::Deny)
            .unwrap());
        assert_eq!(
            auth(
                &mut core,
                McpPermissionLevel::Full,
                "delete_message",
                McpPermissionLevel::Full,
                true,
                "delete:john:thread:1",
            ),
            McpAuthorization::Denied
        );
        let grants: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM mcp_permissions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(grants, 0);
    }

    #[test]
    fn allow_once_send_consumes_and_freezes_in_one_transaction() {
        let mut core = enabled_core();
        set_stdio_level(&mut core, McpPermissionLevel::Send);
        let (account_id, draft_id) = draft(&core);
        let McpSendOutcome::NeedsConfirmation(request) = core
            .queue_mcp_draft_send("stdio", McpPermissionLevel::Send, &account_id, &draft_id)
            .unwrap()
        else {
            panic!("send needs GTK approval");
        };
        assert!(core
            .resolve_mcp_confirmation(request.id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        assert!(matches!(
            core.queue_mcp_draft_send("stdio", McpPermissionLevel::Send, &account_id, &draft_id,)
                .unwrap(),
            McpSendOutcome::Queued(_)
        ));
        let count: i64 = core
            .db
            .conn()
            .query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn changed_draft_revision_cannot_consume_an_old_send_approval_or_queue() {
        let mut core = enabled_core();
        set_stdio_level(&mut core, McpPermissionLevel::Send);
        let (account_id, draft_id) = draft(&core);
        let McpSendOutcome::NeedsConfirmation(request) = core
            .queue_mcp_draft_send("stdio", McpPermissionLevel::Send, &account_id, &draft_id)
            .unwrap()
        else {
            panic!("send needs GTK approval");
        };
        assert!(core
            .resolve_mcp_confirmation(request.id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        core.save_draft(
            &account_id,
            Some(&draft_id),
            DraftContent {
                from: "john@example.test".into(),
                to: "recipient@example.test".into(),
                subject: "edited".into(),
                body: "changed".into(),
                ..DraftContent::default()
            },
        )
        .unwrap();
        assert!(matches!(
            core.queue_mcp_draft_send("stdio", McpPermissionLevel::Send, &account_id, &draft_id,)
                .unwrap(),
            McpSendOutcome::NeedsConfirmation(_)
        ));
        let queued: i64 = core
            .db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE op = 'send'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(queued, 0);
    }

    #[test]
    fn always_send_is_still_limited_by_the_live_environment_ceiling() {
        let mut core = enabled_core();
        set_stdio_level(&mut core, McpPermissionLevel::Send);
        let (account_id, draft_id) = draft(&core);
        let McpSendOutcome::NeedsConfirmation(request) = core
            .queue_mcp_draft_send("stdio", McpPermissionLevel::Send, &account_id, &draft_id)
            .unwrap()
        else {
            panic!("send needs GTK approval");
        };
        assert!(core
            .resolve_mcp_confirmation(request.id, McpConfirmationChoice::AlwaysAllow)
            .unwrap());
        assert_eq!(
            core.queue_mcp_draft_send("stdio", McpPermissionLevel::Draft, &account_id, &draft_id,)
                .unwrap(),
            McpSendOutcome::Denied
        );
    }
}
