//! MCP stdio contract over Feather Mail Core (T-058…T-065).
//!
//! This crate has no GTK and never opens a provider connection. Mutations go
//! through the same durable Core command bus as the UI.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use feathermail_attachments::stream_to_file;
use feathermail_core::body::default_attachments_dir;
use feathermail_core::{
    AccountId, Attachment, AttachmentId, Command, Core, CoreError, DraftContent, DraftId,
    ErrorCode, FolderId, FolderKind, ListThreadsQuery, McpAuthorization, McpBulkHighRiskOutcome,
    McpFolderDeleteOutcome, McpSendOutcome, MessageId, ResponseKind, Thread, ThreadCursor,
    ThreadFilter, ThreadId, UndoReceipt,
};
use feathermail_search::Query;
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// One MCP bulk request maps to one Core transaction and can create one
/// durable operation per target. Keep the stdio boundary deliberately smaller
/// than the GTK selection path while still making a useful batch possible.
const MAX_BULK_THREAD_IDS: usize = 100;

/// Kept as the MCP crate's public spelling for compatibility; authority lives
/// in Core, where persistent policy and GTK confirmation are resolved.
pub use feathermail_core::McpPermissionLevel as PermissionLevel;

#[derive(Clone, Debug)]
pub struct Access {
    pub client_id: String,
    /// Launch configuration can only narrow a durable Core policy; it never
    /// grants access itself.  The default matches D57's Read + Draft level.
    pub ceiling: PermissionLevel,
    /// Empty means no extra narrowing. A non-empty set is a hard account
    /// ceiling and is checked before Core can create a confirmation request.
    pub accounts: HashSet<String>,
    /// Safe file root for draft input and incoming-attachment export. MCP
    /// never exposes Feather Mail's private cache path to the caller.
    pub attachment_root: Option<PathBuf>,
}

impl Default for Access {
    fn default() -> Self {
        Self {
            client_id: "stdio".into(),
            ceiling: PermissionLevel::Draft,
            accounts: HashSet::new(),
            attachment_root: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpError {
    pub code: String,
    pub message: String,
    pending_confirmation: Option<i64>,
}

impl From<CoreError> for McpError {
    fn from(error: CoreError) -> Self {
        Self {
            code: error.code.as_str().into(),
            message: error.message,
            pending_confirmation: None,
        }
    }
}

impl McpError {
    fn permission() -> Self {
        CoreError::from_code(ErrorCode::PermissionDenied).into()
    }

    fn invalid(message: &str) -> Self {
        CoreError::new(ErrorCode::InvalidArgument, message).into()
    }

    fn confirmation(request_id: i64) -> Self {
        Self {
            code: ErrorCode::PermissionDenied.as_str().into(),
            message: "This action needs approval in Feather Mail.".into(),
            pending_confirmation: Some(request_id),
        }
    }

    pub fn pending_confirmation(&self) -> Option<i64> {
        self.pending_confirmation
    }
}

pub fn tool_definitions() -> Vec<Value> {
    let account = json!({"type":"object","properties":{"account_id":{"type":"string"}},"required":["account_id"],"additionalProperties":true});
    let account_thread = json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_id":{"type":"string"}},"required":["account_id","thread_id"],"additionalProperties":false});
    let cursor = cursor_schema();
    let mut tools = Vec::new();
    let mut add = |name: &str, description: &str, schema: Value| {
        tools.push(json!({"name":name,"description":description,"inputSchema":schema}));
    };
    add(
        "list_accounts",
        "List local mail accounts.",
        json!({"type":"object","additionalProperties":false}),
    );
    add("get_account", "Get one local account.", account.clone());
    add(
        "get_account_status",
        "Get one account's current Core status without account metadata.",
        json!({"type":"object","properties":{"account_id":{"type":"string"}},"required":["account_id"],"additionalProperties":false}),
    );
    add(
        "sync_account",
        "Ask the running Feather Mail window to sync this account now; the request is durable and collapses onto one pending sync per account.",
        account.clone(),
    );
    add(
        "list_folders",
        "List account folders and counts.",
        account.clone(),
    );
    add(
        "get_folder",
        "Get one local folder's identity, kind and current counts.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"folder_id":{"type":"string"}},"required":["account_id","folder_id"],"additionalProperties":false}),
    );
    add(
        "get_folder_message_count",
        "Get the current unread and total counts for one local folder.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"folder_id":{"type":"string"}},"required":["account_id","folder_id"],"additionalProperties":false}),
    );
    add(
        "list_threads",
        "List one folder with typed cursor pagination.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"folder_id":{"type":"string"},"after":cursor.clone(),"limit":{"type":"integer","minimum":1,"maximum":200}},"required":["account_id","folder_id"],"additionalProperties":false}),
    );
    add(
        "list_snoozed",
        "List currently snoozed local threads with typed cursor pagination.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"after":cursor.clone(),"limit":{"type":"integer","minimum":1,"maximum":200}},"required":["account_id"],"additionalProperties":false}),
    );
    add(
        "get_thread",
        "Get a thread and message metadata; bodies are excluded.",
        account_thread.clone(),
    );
    add(
        "list_thread_messages",
        "List message metadata in one thread; bodies are excluded.",
        account_thread.clone(),
    );
    add(
        "get_message",
        "Get one local message's metadata; bodies are excluded.",
        message_id_schema(),
    );
    add(
        "get_messages",
        "Get metadata for up to 100 local messages in one account; bodies are excluded.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"message_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_BULK_THREAD_IDS,"uniqueItems":true}},"required":["account_id","message_ids"],"additionalProperties":false}),
    );
    add(
        "search_mail",
        "Search with the same parser and typed cursor pagination as the UI.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"query":{"type":"string"},"after":cursor,"limit":{"type":"integer","minimum":1,"maximum":200}},"required":["account_id","query"],"additionalProperties":false}),
    );
    for name in [
        "archive_message",
        "mark_read",
        "mark_unread",
        "star_message",
        "unstar_message",
    ] {
        add(
            name,
            "Queue the mail mutation through Core.",
            account_thread.clone(),
        );
    }
    add(
        "bulk_mark_read",
        "Mark up to 100 local threads read through the same queued Core command as GTK selection.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_BULK_THREAD_IDS,"uniqueItems":true}},"required":["account_id","thread_ids"],"additionalProperties":false}),
    );
    add(
        "bulk_mark_unread",
        "Mark up to 100 local threads unread through the same queued Core command as GTK selection.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_BULK_THREAD_IDS,"uniqueItems":true}},"required":["account_id","thread_ids"],"additionalProperties":false}),
    );
    add(
        "bulk_archive",
        "Archive up to 100 local threads through the same queued Core command as GTK selection.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_BULK_THREAD_IDS,"uniqueItems":true}},"required":["account_id","thread_ids"],"additionalProperties":false}),
    );
    add(
        "bulk_star",
        "Star up to 100 local threads through the same Core command as GTK.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_BULK_THREAD_IDS,"uniqueItems":true}},"required":["account_id","thread_ids"],"additionalProperties":false}),
    );
    add(
        "bulk_unstar",
        "Unstar up to 100 local threads through the same Core command as GTK.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_BULK_THREAD_IDS,"uniqueItems":true}},"required":["account_id","thread_ids"],"additionalProperties":false}),
    );
    add(
        "bulk_move",
        "Move up to 100 local threads to an existing custom folder through the same queued Core command as GTK selection.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"folder_id":{"type":"string"},"thread_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_BULK_THREAD_IDS,"uniqueItems":true}},"required":["account_id","folder_id","thread_ids"],"additionalProperties":false}),
    );
    add(
        "bulk_snooze",
        "Snooze up to 100 local threads until one Unix timestamp through the same Core command as GTK selection.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_BULK_THREAD_IDS,"uniqueItems":true},"until":{"type":"integer"}},"required":["account_id","thread_ids","until"],"additionalProperties":false}),
    );
    add(
        "bulk_delete",
        "Move up to 100 local threads to Trash after Feather Mail confirmation.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_BULK_THREAD_IDS,"uniqueItems":true}},"required":["account_id","thread_ids"],"additionalProperties":false}),
    );
    add(
        "bulk_permanent_delete",
        "Permanently delete up to 100 local threads after Feather Mail confirmation.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_ids":{"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":MAX_BULK_THREAD_IDS,"uniqueItems":true}},"required":["account_id","thread_ids"],"additionalProperties":false}),
    );
    add(
        "delete_message",
        "Move a thread to Trash (confirmation required).",
        account_thread.clone(),
    );
    add(
        "permanent_delete",
        "Permanently delete a thread through Core (confirmation required).",
        account_thread.clone(),
    );
    add(
        "restore_message",
        "Restore one locally trashed thread through its exact Core Trash lifecycle.",
        account_thread.clone(),
    );
    add(
        "snooze_message",
        "Snooze a thread until a Unix timestamp.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_id":{"type":"string"},"until":{"type":"integer"}},"required":["account_id","thread_id","until"],"additionalProperties":false}),
    );
    add(
        "unsnooze_message",
        "Bring a snoozed thread back to Inbox now, exactly as its own timer would have.",
        account_thread.clone(),
    );
    add(
        "move_message",
        "Move a thread to an existing custom local folder through Core.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_id":{"type":"string"},"folder_id":{"type":"string"}},"required":["account_id","thread_id","folder_id"],"additionalProperties":false}),
    );
    add(
        "create_folder",
        "Create a folder locally and queue IMAP CREATE.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"name":{"type":"string"}},"required":["account_id","name"],"additionalProperties":false}),
    );
    add(
        "rename_folder",
        "Rename one custom folder and queue IMAP RENAME; the folder keeps its mail and its place in the hierarchy.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"folder_id":{"type":"string"},"name":{"type":"string"}},"required":["account_id","folder_id","name"],"additionalProperties":false}),
    );
    add(
        "delete_folder",
        "Delete one empty custom folder and queue IMAP DELETE; refused while the folder still holds mail, and always needs approval in Feather Mail.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"folder_id":{"type":"string"}},"required":["account_id","folder_id"],"additionalProperties":false}),
    );
    add("list_drafts", "List local unsent drafts.", account.clone());
    add("get_draft", "Read one local draft.", draft_id_schema());
    add("create_draft", "Create a local draft.", draft_schema(false));
    add("update_draft", "Update a local draft.", draft_schema(true));
    add("delete_draft", "Discard a local draft.", draft_id_schema());
    add(
        "reply_to_thread",
        "Create a local Reply or Reply all draft for a thread; send remains separate.",
        reply_schema(),
    );
    add(
        "forward_message",
        "Create a local Forward draft for one message; send remains separate.",
        message_id_schema(),
    );
    add(
        "send_draft",
        "Queue a saved draft for SMTP after Feather Mail user approval.",
        draft_id_schema(),
    );
    add(
        "send_email",
        "Save one message as a draft in this account and queue it for sending through the same approval as send_draft.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"to":{"type":"string"},"cc":{"type":"string"},"bcc":{"type":"string"},"subject":{"type":"string"},"body":{"type":"string"},"thread_id":{"type":"string"},"reply_to_message_id":{"type":"string"}},"required":["account_id","to"],"additionalProperties":false}),
    );
    add(
        "attach_file_to_draft",
        "Attach a file under the configured safe root.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"draft_id":{"type":"string"},"path":{"type":"string"}},"required":["account_id","draft_id","path"],"additionalProperties":false}),
    );
    add(
        "remove_attachment_from_draft",
        "Remove one draft attachment.",
        json!({"type":"object","properties":{"account_id":{"type":"string"},"draft_id":{"type":"string"},"attachment_id":{"type":"string"}},"required":["account_id","draft_id","attachment_id"],"additionalProperties":false}),
    );
    add(
        "list_attachments",
        "List metadata for incoming attachments on one message; never returns bytes or cache paths.",
        message_id_schema(),
    );
    add(
        "get_attachment",
        "Get metadata and local-cache state for one incoming attachment; never returns bytes or cache paths.",
        attachment_id_schema(),
    );
    add(
        "download_attachment",
        "Stream a locally downloaded incoming attachment into the configured safe root using its safe filename.",
        attachment_id_schema(),
    );
    add(
        "save_attachment",
        "Stream a locally downloaded incoming attachment to a new file beneath the configured safe root.",
        attachment_save_schema(),
    );
    add(
        "list_draft_attachments",
        "List metadata for attachments on a draft.",
        draft_id_schema(),
    );
    tools
}

fn draft_id_schema() -> Value {
    json!({"type":"object","properties":{"account_id":{"type":"string"},"draft_id":{"type":"string"}},"required":["account_id","draft_id"],"additionalProperties":false})
}

fn message_id_schema() -> Value {
    json!({"type":"object","properties":{"account_id":{"type":"string"},"message_id":{"type":"string"}},"required":["account_id","message_id"],"additionalProperties":false})
}

fn reply_schema() -> Value {
    json!({"type":"object","properties":{"account_id":{"type":"string"},"thread_id":{"type":"string"},"reply_all":{"type":"boolean"}},"required":["account_id","thread_id"],"additionalProperties":false})
}

fn attachment_id_schema() -> Value {
    json!({"type":"object","properties":{"account_id":{"type":"string"},"attachment_id":{"type":"string"}},"required":["account_id","attachment_id"],"additionalProperties":false})
}

fn attachment_save_schema() -> Value {
    json!({"type":"object","properties":{"account_id":{"type":"string"},"attachment_id":{"type":"string"},"path":{"type":"string"}},"required":["account_id","attachment_id","path"],"additionalProperties":false})
}

fn cursor_schema() -> Value {
    json!({"type":"object","properties":{"date":{"type":"integer"},"id":{"type":"string"}},"required":["date","id"],"additionalProperties":false})
}

fn draft_schema(update: bool) -> Value {
    let mut required = vec!["account_id", "from"];
    if update {
        required.push("draft_id");
    }
    json!({"type":"object","properties":{"account_id":{"type":"string"},"draft_id":{"type":"string"},"from":{"type":"string"},"to":{"type":"string"},"cc":{"type":"string"},"bcc":{"type":"string"},"subject":{"type":"string"},"body":{"type":"string"},"thread_id":{"type":"string"},"in_reply_to":{"type":"string"}},"required":required,"additionalProperties":false})
}

pub fn call_tool(
    core: &mut Core,
    access: &Access,
    name: &str,
    args: &Value,
) -> Result<Value, McpError> {
    let account = args.get("account_id").and_then(Value::as_str);
    let outcome = match name {
        "send_draft" => send_draft_tool(core, access, args),
        "send_email" => send_email_tool(core, access, args),
        "bulk_delete" => bulk_delete_tool(core, access, args),
        "bulk_permanent_delete" => bulk_permanent_delete_tool(core, access, args),
        "delete_folder" => delete_folder_tool(core, access, args),
        _ => authorize_tool(core, access, name, args, account)
            .and_then(|()| call_tool_inner(core, access, name, args)),
    };
    // Audit only canonical tool names. Account metadata is useful after a
    // successful Core action, but failed/unknown requests must not turn their
    // caller-controlled identifiers into durable data. Targets are never
    // persisted: even valid-looking target strings may be submitted content.
    let audit_account = outcome
        .as_ref()
        .ok()
        .and(account)
        .map(|value| AccountId(value.into()));
    let _ = core.record_mcp_audit(
        &access.client_id,
        canonical_audit_tool(name),
        audit_account.as_ref(),
        if outcome.is_ok() {
            "ok"
        } else {
            "denied_or_error"
        },
    );
    outcome
}

fn canonical_audit_tool(name: &str) -> &'static str {
    match name {
        "list_accounts" => "list_accounts",
        "get_account" => "get_account",
        "get_account_status" => "get_account_status",
        "sync_account" => "sync_account",
        "list_folders" => "list_folders",
        "get_folder" => "get_folder",
        "get_folder_message_count" => "get_folder_message_count",
        "list_threads" => "list_threads",
        "list_snoozed" => "list_snoozed",
        "get_thread" => "get_thread",
        "list_thread_messages" => "list_thread_messages",
        "get_message" => "get_message",
        "get_messages" => "get_messages",
        "search_mail" => "search_mail",
        "archive_message" => "archive_message",
        "bulk_archive" => "bulk_archive",
        "bulk_star" => "bulk_star",
        "bulk_unstar" => "bulk_unstar",
        "mark_read" => "mark_read",
        "bulk_mark_read" => "bulk_mark_read",
        "mark_unread" => "mark_unread",
        "bulk_mark_unread" => "bulk_mark_unread",
        "star_message" => "star_message",
        "unstar_message" => "unstar_message",
        "delete_message" => "delete_message",
        "permanent_delete" => "permanent_delete",
        "restore_message" => "restore_message",
        "snooze_message" => "snooze_message",
        "unsnooze_message" => "unsnooze_message",
        "move_message" => "move_message",
        "bulk_move" => "bulk_move",
        "bulk_snooze" => "bulk_snooze",
        "bulk_delete" => "bulk_delete",
        "bulk_permanent_delete" => "bulk_permanent_delete",
        "create_folder" => "create_folder",
        "rename_folder" => "rename_folder",
        "delete_folder" => "delete_folder",
        "list_drafts" => "list_drafts",
        "get_draft" => "get_draft",
        "create_draft" => "create_draft",
        "update_draft" => "update_draft",
        "delete_draft" => "delete_draft",
        "reply_to_thread" => "reply_to_thread",
        "forward_message" => "forward_message",
        "send_draft" => "send_draft",
        "send_email" => "send_email",
        "attach_file_to_draft" => "attach_file_to_draft",
        "remove_attachment_from_draft" => "remove_attachment_from_draft",
        "list_attachments" => "list_attachments",
        "get_attachment" => "get_attachment",
        "download_attachment" => "download_attachment",
        "save_attachment" => "save_attachment",
        "list_draft_attachments" => "list_draft_attachments",
        _ => "unknown",
    }
}

#[derive(Clone, Copy)]
struct ToolPolicy {
    capability: CapabilityClass,
    required: PermissionLevel,
}

/// The canonical R/D/W/M/H vocabulary from capability-matrix.md. D57's four
/// levels intentionally collapse D/W/M into the Draft level; keeping the
/// original class here prevents a future tool from silently losing its matrix
/// classification when that policy is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CapabilityClass {
    Read,
    Draft,
    Write,
    Modify,
    High,
}

/// Explicitly maps every currently registered tool to D57's four profiles.
/// Read covers R; Draft covers D/W/M; Send and Full are the two H actions and
/// never become implied grants (they still need an exact persistent allowance
/// or a GTK Allow-once decision).
fn tool_policy(name: &str) -> Option<ToolPolicy> {
    let (capability, required) = match name {
        "list_accounts"
        | "get_account"
        | "get_account_status"
        | "list_folders"
        | "get_folder"
        | "get_folder_message_count"
        | "list_threads"
        | "list_snoozed"
        | "get_thread"
        | "list_thread_messages"
        | "get_message"
        | "get_messages"
        | "search_mail"
        | "list_drafts"
        | "get_draft"
        | "list_attachments"
        | "get_attachment"
        | "download_attachment"
        | "save_attachment"
        | "list_draft_attachments" => (CapabilityClass::Read, PermissionLevel::Read),
        "create_draft"
        | "update_draft"
        | "delete_draft"
        | "reply_to_thread"
        | "forward_message"
        | "attach_file_to_draft"
        | "remove_attachment_from_draft" => (CapabilityClass::Draft, PermissionLevel::Draft),
        "archive_message" | "bulk_archive" | "mark_read" | "bulk_mark_read" | "mark_unread"
        | "bulk_mark_unread" | "star_message" | "unstar_message" | "bulk_star" | "bulk_unstar"
        | "sync_account" => (CapabilityClass::Write, PermissionLevel::Draft),
        "snooze_message" | "unsnooze_message" | "bulk_snooze" | "move_message" | "bulk_move"
        | "restore_message" | "create_folder" | "rename_folder" => {
            (CapabilityClass::Modify, PermissionLevel::Draft)
        }
        "send_draft" | "send_email" => (CapabilityClass::High, PermissionLevel::Send),
        "delete_message"
        | "permanent_delete"
        | "bulk_delete"
        | "bulk_permanent_delete"
        | "delete_folder" => (CapabilityClass::High, PermissionLevel::Full),
        _ => return None,
    };
    Some(ToolPolicy {
        capability,
        required,
    })
}

fn authorize_tool(
    core: &mut Core,
    access: &Access,
    name: &str,
    args: &Value,
    account: Option<&str>,
) -> Result<(), McpError> {
    let Some(policy) = tool_policy(name) else {
        return Err(McpError::invalid("Unknown MCP tool."));
    };
    let needs_confirmation = policy.capability == CapabilityClass::High;
    if needs_confirmation && args.get("confirm").is_some() {
        return Err(McpError::invalid(
            "MCP cannot confirm this action; approve it in Feather Mail.",
        ));
    }
    if let Some(account) = account {
        require_account(access, account)?;
    }
    let account_id = account.map(|value| AccountId(value.into()));
    let target_id = args
        .get("thread_id")
        .or_else(|| args.get("attachment_id"))
        .or_else(|| args.get("draft_id"))
        .or_else(|| args.get("message_id"))
        .and_then(Value::as_str);
    let fingerprint = confirmation_fingerprint(name, account_id.as_ref(), target_id);
    match core
        .authorize_mcp_action(
            &access.client_id,
            access.ceiling,
            name,
            policy.required,
            needs_confirmation,
            account_id.as_ref(),
            target_id,
            &fingerprint,
        )
        .map_err(McpError::from)?
    {
        McpAuthorization::Allowed => Ok(()),
        McpAuthorization::NeedsConfirmation(request) => Err(McpError::confirmation(request.id)),
        McpAuthorization::Denied => Err(McpError::permission()),
    }
}

fn confirmation_fingerprint(
    name: &str,
    account_id: Option<&AccountId>,
    target_id: Option<&str>,
) -> String {
    format!(
        "{name}:{}:{}",
        account_id.map(AccountId::as_str).unwrap_or("none"),
        target_id.unwrap_or("none")
    )
}

/// Send deliberately bypasses the generic authorize-then-call shape.  Core
/// consumes the specific approval and freezes the exact draft revision in one
/// transaction; doing those two steps here would reintroduce a TOCTOU gap.
fn send_draft_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    if args.get("confirm").is_some() {
        return Err(McpError::invalid(
            "MCP cannot confirm this action; approve it in Feather Mail.",
        ));
    }
    let account_id = account_arg(args)?;
    require_account(access, account_id.as_str())?;
    let draft_id = DraftId(string_arg(args, "draft_id")?.into());
    match core
        .queue_mcp_draft_send(&access.client_id, access.ceiling, &account_id, &draft_id)
        .map_err(McpError::from)?
    {
        McpSendOutcome::Queued(operation_id) => {
            Ok(json!({"queued":true,"operation_id":operation_id.as_str()}))
        }
        McpSendOutcome::NeedsConfirmation(request) => Err(McpError::confirmation(request.id)),
        McpSendOutcome::Denied => Err(McpError::permission()),
    }
}

/// `send_email` is the one-call form of the two-step model: it saves exactly
/// the message it is about to send as a normal local draft, then goes through
/// the same `send_draft` Core door and the same GTK approval. The user always
/// has the real message in Drafts to read before approving, and nothing is
/// ever sent without that approval.
///
/// The draft id is derived from the message content, so repeating the call
/// while the approval is pending reuses the same draft row and the same draft
/// revision -- and therefore the same pending approval -- instead of piling up
/// near-identical drafts that each need their own confirmation. Changing any
/// field is a different message and correctly needs its own approval.
fn send_email_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    if args.get("confirm").is_some() {
        return Err(McpError::invalid(
            "MCP cannot confirm this action; approve it in Feather Mail.",
        ));
    }
    let account_id = account_arg(args)?;
    require_account(access, account_id.as_str())?;
    require_level(access, PermissionLevel::Send)?;
    // Refuse before writing anything local when the durable Settings policy
    // would refuse the send anyway. `queue_mcp_draft_send` re-checks this
    // under its own transaction and stays authoritative.
    if !core.mcp_client_allows(&access.client_id, PermissionLevel::Send)? {
        return Err(McpError::permission());
    }

    // From is the account's own address. MCP never chooses a sender identity.
    let from = core
        .list_accounts()?
        .into_iter()
        .find(|account| account.id == account_id)
        .ok_or_else(|| McpError::from(CoreError::from_code(ErrorCode::AccountNotFound)))?
        .email;
    let content = DraftContent {
        thread_id: args
            .get("thread_id")
            .and_then(Value::as_str)
            .map(|s| ThreadId(s.into())),
        in_reply_to: args
            .get("reply_to_message_id")
            .and_then(Value::as_str)
            .map(|s| MessageId(s.into())),
        from,
        to: string_arg(args, "to")?.into(),
        cc: optional_string(args, "cc"),
        bcc: optional_string(args, "bcc"),
        subject: optional_string(args, "subject"),
        body: optional_string(args, "body"),
    };
    let draft_id = DraftId(format!(
        "draft:{}:send-email:{}",
        account_id.as_str(),
        send_email_digest(&account_id, &content)
    ));
    // Saving is an upsert that bumps the draft's send revision, which is part
    // of the approval fingerprint. Only write when the stored draft is not
    // already this exact message.
    if !core
        .get_draft(&account_id, &draft_id)
        .is_ok_and(|stored| draft_matches_content(&stored, &content))
    {
        core.save_draft(&account_id, Some(&draft_id), content)?;
    }

    match core
        .queue_mcp_draft_send(&access.client_id, access.ceiling, &account_id, &draft_id)
        .map_err(McpError::from)?
    {
        McpSendOutcome::Queued(operation_id) => Ok(
            json!({"queued":true,"draft_id":draft_id.as_str(),"operation_id":operation_id.as_str()}),
        ),
        McpSendOutcome::NeedsConfirmation(request) => Err(McpError::confirmation(request.id)),
        McpSendOutcome::Denied => Err(McpError::permission()),
    }
}

/// Length-framed digest of exactly the fields that make up the message, so no
/// separator ambiguity can make two different messages share one draft row and
/// therefore one approval. `save_draft` trims the same fields it stores, so
/// the digest is taken over the trimmed values it will actually keep.
fn send_email_digest(account_id: &AccountId, content: &DraftContent) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut field = |value: &str| {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    };
    field(account_id.as_str());
    field(
        content
            .thread_id
            .as_ref()
            .map(ThreadId::as_str)
            .unwrap_or(""),
    );
    field(
        content
            .in_reply_to
            .as_ref()
            .map(MessageId::as_str)
            .unwrap_or(""),
    );
    field(content.from.trim());
    field(content.to.trim());
    field(content.cc.trim());
    field(content.bcc.trim());
    field(content.subject.trim());
    field(content.body.trim());
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// True when the stored draft already is this exact message, in the same
/// trimmed form `save_draft` persists.
fn draft_matches_content(stored: &feathermail_core::Draft, content: &DraftContent) -> bool {
    stored.thread_id == content.thread_id
        && stored.in_reply_to == content.in_reply_to
        && stored.from == content.from.trim()
        && stored.to == content.to.trim()
        && stored.cc == content.cc.trim()
        && stored.bcc == content.bcc.trim()
        && stored.subject == content.subject.trim()
        && stored.body == content.body.trim()
}

fn call_tool_inner(
    core: &mut Core,
    access: &Access,
    name: &str,
    args: &Value,
) -> Result<Value, McpError> {
    match name {
        "list_accounts" => {
            require_level(access, PermissionLevel::Read)?;
            let accounts = core.list_accounts()?.into_iter().filter(|a| access.accounts.is_empty() || access.accounts.contains(a.id.as_str())).map(|a| json!({"id":a.id.as_str(),"name":a.name,"email":a.email,"status":a.status.as_str()})).collect::<Vec<_>>();
            Ok(json!({"accounts":accounts}))
        }
        "get_account" => {
            require_level(access, PermissionLevel::Read)?;
            let id = account_arg(args)?;
            let account = core
                .list_accounts()?
                .into_iter()
                .find(|a| a.id == id)
                .ok_or_else(|| McpError::from(CoreError::from_code(ErrorCode::AccountNotFound)))?;
            Ok(
                json!({"id":account.id.as_str(),"name":account.name,"email":account.email,"status":account.status.as_str()}),
            )
        }
        "get_account_status" => {
            require_level(access, PermissionLevel::Read)?;
            let id = account_arg(args)?;
            let account = core
                .list_accounts()?
                .into_iter()
                .find(|account| account.id == id)
                .ok_or_else(|| McpError::from(CoreError::from_code(ErrorCode::AccountNotFound)))?;
            Ok(json!({"account_id":account.id.as_str(),"status":account.status.as_str()}))
        }
        // T-060s: this process holds no `SyncHandle` -- the sync worker's
        // channel lives in the GTK shell. The request goes into Core, and
        // the shell claims it on the poll it already runs for pending
        // confirmations, then wakes the worker exactly like Diagnostics
        // "Sync now". `queued` is therefore the honest word: with no window
        // running, the sync happens when one next opens.
        "sync_account" => {
            require_level(access, PermissionLevel::Draft)?;
            let account = account_arg(args)?;
            core.request_account_sync(&account)?;
            Ok(json!({"queued":true,"account_id":account.as_str()}))
        }
        "list_folders" => {
            require_level(access, PermissionLevel::Read)?;
            let folders = core
                .list_folders(&account_arg(args)?)?
                .iter()
                .map(folder_json)
                .collect::<Vec<_>>();
            Ok(json!({"folders":folders}))
        }
        "get_folder" => {
            require_level(access, PermissionLevel::Read)?;
            let account = account_arg(args)?;
            let folder_id = string_arg(args, "folder_id")?;
            let folder = core
                .list_folders(&account)?
                .into_iter()
                .find(|folder| folder.folder.id.as_str() == folder_id)
                .ok_or_else(|| {
                    McpError::invalid("folder_id does not name a folder in this account.")
                })?;
            Ok(folder_json(&folder))
        }
        "get_folder_message_count" => {
            require_level(access, PermissionLevel::Read)?;
            let account = account_arg(args)?;
            let folder_id = string_arg(args, "folder_id")?;
            let folder = core
                .list_folders(&account)?
                .into_iter()
                .find(|folder| folder.folder.id.as_str() == folder_id)
                .ok_or_else(|| {
                    McpError::invalid("folder_id does not name a folder in this account.")
                })?;
            Ok(
                json!({"folder_id":folder.folder.id.as_str(),"unread":folder.unread,"total":folder.total}),
            )
        }
        "list_threads" => {
            require_level(access, PermissionLevel::Read)?;
            let page = core.list_threads(ListThreadsQuery {
                account_id: account_arg(args)?,
                folder_id: FolderId(string_arg(args, "folder_id")?.into()),
                filter: ThreadFilter::All,
                after: cursor_arg(args)?,
                limit: limit_arg(args)?,
            })?;
            Ok(
                json!({"threads":page.threads.iter().map(thread_json).collect::<Vec<_>>(),"total":page.total,"next":page.next.as_ref().map(cursor_json)}),
            )
        }
        "list_snoozed" => {
            require_level(access, PermissionLevel::Read)?;
            let page = core.list_threads(ListThreadsQuery {
                account_id: account_arg(args)?,
                folder_id: FolderId("snoozed".into()),
                filter: ThreadFilter::All,
                after: cursor_arg(args)?,
                limit: limit_arg(args)?,
            })?;
            Ok(
                json!({"threads":page.threads.iter().map(thread_json).collect::<Vec<_>>(),"total":page.total,"next":page.next.as_ref().map(cursor_json)}),
            )
        }
        "get_thread" => {
            require_level(access, PermissionLevel::Read)?;
            let account = account_arg(args)?;
            let thread_id = ThreadId(string_arg(args, "thread_id")?.into());
            let thread = core.get_thread(&account, &thread_id)?;
            let messages = core
                .list_thread_messages(&account, &thread_id)?
                .iter()
                .map(thread_message_json)
                .collect::<Vec<_>>();
            Ok(json!({"thread":thread_json(&thread),"messages":messages}))
        }
        "list_thread_messages" => {
            require_level(access, PermissionLevel::Read)?;
            let account = account_arg(args)?;
            let thread_id = ThreadId(string_arg(args, "thread_id")?.into());
            let messages = core
                .list_thread_messages(&account, &thread_id)?
                .iter()
                .map(thread_message_json)
                .collect::<Vec<_>>();
            Ok(json!({"messages":messages}))
        }
        "get_message" => {
            require_level(access, PermissionLevel::Read)?;
            let account = account_arg(args)?;
            let message_id = MessageId(string_arg(args, "message_id")?.into());
            let message = core.get_thread_message(&account, &message_id)?;
            Ok(json!({"message":message_metadata_json(&message)}))
        }
        "get_messages" => {
            require_level(access, PermissionLevel::Read)?;
            let account = account_arg(args)?;
            let message_ids = bulk_message_ids_arg(args)?;
            // Atomic read: one unknown or foreign id fails the whole call, so
            // a caller can never mistake a short array for "these are all the
            // messages that exist". The single-message door stays the only
            // place that decides what a message is allowed to expose.
            let mut messages = Vec::with_capacity(message_ids.len());
            for message_id in &message_ids {
                messages.push(message_metadata_json(
                    &core.get_thread_message(&account, message_id)?,
                ));
            }
            Ok(json!({"messages":messages}))
        }
        "search_mail" => {
            require_level(access, PermissionLevel::Read)?;
            let plan = Query::parse(string_arg(args, "query")?).to_search_plan();
            let after = cursor_arg(args)?;
            let results =
                core.search(&account_arg(args)?, &plan, after.as_ref(), limit_arg(args)?)?;
            Ok(
                json!({"threads":results.threads.iter().map(thread_json).collect::<Vec<_>>(),"pending_index":results.pending_index,"next":results.next.as_ref().map(cursor_json)}),
            )
        }
        "archive_message" | "mark_read" | "mark_unread" | "star_message" | "unstar_message"
        | "delete_message" | "permanent_delete" | "snooze_message" | "move_message" => {
            dispatch_tool(core, access, name, args)
        }
        "bulk_mark_read" => bulk_mark_read_tool(core, access, args),
        "bulk_mark_unread" => bulk_mark_unread_tool(core, access, args),
        "bulk_archive" => bulk_archive_tool(core, access, args),
        "bulk_star" => bulk_star_tool(core, access, args),
        "bulk_unstar" => bulk_unstar_tool(core, access, args),
        "bulk_move" => bulk_move_tool(core, access, args),
        "bulk_snooze" => bulk_snooze_tool(core, access, args),
        "restore_message" => restore_message_tool(core, access, args),
        "unsnooze_message" => {
            require_level(access, PermissionLevel::Draft)?;
            let account_id = account_arg(args)?;
            let thread_id = ThreadId(string_arg(args, "thread_id")?.into());
            // `false` is not an error: the caller asked for an end state that
            // already holds. Saying so plainly beats inventing a failure.
            let unsnoozed = core.unsnooze_thread(&account_id, &thread_id)?;
            Ok(json!({"unsnoozed":unsnoozed,"queued":false}))
        }
        "create_folder" => {
            require_level(access, PermissionLevel::Draft)?;
            let id = core.create_folder(&account_arg(args)?, string_arg(args, "name")?)?;
            Ok(json!({"folder_id":id.as_str(),"queued":true}))
        }
        "rename_folder" => {
            require_level(access, PermissionLevel::Draft)?;
            let account_id = account_arg(args)?;
            let folder_id = FolderId(string_arg(args, "folder_id")?.into());
            // Core decides the destination mailbox path; the caller only ever
            // names the leaf. Renaming to the current name is a no-op there,
            // so `queued` is what the caller should read, not the call itself.
            let queued = core.rename_folder(&account_id, &folder_id, string_arg(args, "name")?)?;
            Ok(json!({"folder_id":folder_id.as_str(),"queued":queued}))
        }
        "list_drafts" => {
            require_level(access, PermissionLevel::Read)?;
            Ok(
                json!({"drafts":core.list_drafts(&account_arg(args)?)?.iter().map(draft_metadata_json).collect::<Vec<_>>()}),
            )
        }
        "get_draft" => {
            require_level(access, PermissionLevel::Read)?;
            let d = core.get_draft(
                &account_arg(args)?,
                &DraftId(string_arg(args, "draft_id")?.into()),
            )?;
            Ok(draft_metadata_json(&d))
        }
        "create_draft" | "update_draft" => save_draft_tool(core, access, args),
        "reply_to_thread" => {
            require_level(access, PermissionLevel::Draft)?;
            let account_id = account_arg(args)?;
            let thread_id = ThreadId(string_arg(args, "thread_id")?.into());
            let message_id = core
                .get_thread(&account_id, &thread_id)?
                .message_id
                .ok_or_else(|| McpError::from(CoreError::from_code(ErrorCode::MessageNotFound)))?;
            let kind = if args.get("reply_all") == Some(&Value::Bool(true)) {
                ResponseKind::ReplyAll
            } else {
                ResponseKind::Reply
            };
            Ok(draft_metadata_json(&core.create_response_draft(
                &account_id,
                &message_id,
                kind,
                String::new(),
            )?))
        }
        "forward_message" => {
            require_level(access, PermissionLevel::Draft)?;
            let account_id = account_arg(args)?;
            let message_id = MessageId(string_arg(args, "message_id")?.into());
            Ok(draft_metadata_json(&core.create_response_draft(
                &account_id,
                &message_id,
                ResponseKind::Forward,
                String::new(),
            )?))
        }
        "delete_draft" => {
            require_level(access, PermissionLevel::Draft)?;
            let removed = core.delete_draft(
                &account_arg(args)?,
                &DraftId(string_arg(args, "draft_id")?.into()),
            )?;
            Ok(json!({"deleted":removed}))
        }
        "attach_file_to_draft" => {
            require_level(access, PermissionLevel::Draft)?;
            let path = safe_attachment_path(access, string_arg(args, "path")?)?;
            let a = core.attach_to_draft(
                &account_arg(args)?,
                &DraftId(string_arg(args, "draft_id")?.into()),
                &path,
            )?;
            Ok(json!({"id":a.id,"filename":a.filename,"mime":a.mime,"size_bytes":a.size_bytes}))
        }
        "remove_attachment_from_draft" => {
            require_level(access, PermissionLevel::Draft)?;
            let removed = core.remove_draft_attachment(
                &account_arg(args)?,
                &DraftId(string_arg(args, "draft_id")?.into()),
                string_arg(args, "attachment_id")?,
            )?;
            Ok(json!({"removed":removed}))
        }
        "list_attachments" => {
            require_level(access, PermissionLevel::Read)?;
            let a = core
                .list_attachments(
                    &account_arg(args)?,
                    &MessageId(string_arg(args, "message_id")?.into()),
                )?
                .into_iter()
                .map(|attachment| attachment_json(&attachment, attachment_is_cached(&attachment)))
                .collect::<Vec<_>>();
            Ok(json!({"attachments":a}))
        }
        "get_attachment" => {
            require_level(access, PermissionLevel::Read)?;
            let attachment = core.get_attachment(
                &account_arg(args)?,
                &AttachmentId(string_arg(args, "attachment_id")?.into()),
            )?;
            Ok(attachment_json(
                &attachment,
                attachment_is_cached(&attachment),
            ))
        }
        "download_attachment" => {
            require_level(access, PermissionLevel::Read)?;
            let attachment = core.get_attachment(
                &account_arg(args)?,
                &AttachmentId(string_arg(args, "attachment_id")?.into()),
            )?;
            let root = attachment_root(access)?;
            let destination = safe_attachment_destination(
                &root,
                &root.join(safe_attachment_filename(&attachment.filename)),
            )?;
            export_cached_attachment(&attachment, &default_attachments_dir(), &destination)?;
            Ok(json!({"saved":true,"attachment":attachment_json(&attachment, true)}))
        }
        "save_attachment" => {
            require_level(access, PermissionLevel::Read)?;
            let attachment = core.get_attachment(
                &account_arg(args)?,
                &AttachmentId(string_arg(args, "attachment_id")?.into()),
            )?;
            let destination = safe_attachment_destination(
                &attachment_root(access)?,
                Path::new(string_arg(args, "path")?),
            )?;
            export_cached_attachment(&attachment, &default_attachments_dir(), &destination)?;
            Ok(json!({"saved":true,"attachment":attachment_json(&attachment, true)}))
        }
        "list_draft_attachments" => {
            require_level(access, PermissionLevel::Read)?;
            let a=core.list_draft_attachments(&account_arg(args)?,&DraftId(string_arg(args,"draft_id")?.into()))?.into_iter().map(|a|json!({"id":a.id,"filename":a.filename,"mime":a.mime,"size_bytes":a.size_bytes})).collect::<Vec<_>>();
            Ok(json!({"attachments":a}))
        }
        _ => Err(McpError::invalid("Unknown MCP tool.")),
    }
}

fn dispatch_tool(
    core: &mut Core,
    access: &Access,
    name: &str,
    args: &Value,
) -> Result<Value, McpError> {
    let needed = if matches!(name, "delete_message" | "permanent_delete") {
        PermissionLevel::Full
    } else {
        PermissionLevel::Draft
    };
    require_level(access, needed)?;
    let account_id = account_arg(args)?;
    let thread_id = ThreadId(string_arg(args, "thread_id")?.into());
    let command = match name {
        "archive_message" => Command::Archive {
            account_id,
            thread_ids: vec![thread_id],
        },
        "mark_read" => Command::MarkRead {
            account_id,
            thread_ids: vec![thread_id],
        },
        "mark_unread" => Command::MarkUnread {
            account_id,
            thread_ids: vec![thread_id],
        },
        "star_message" => Command::Star {
            account_id,
            thread_ids: vec![thread_id],
        },
        "unstar_message" => Command::Unstar {
            account_id,
            thread_ids: vec![thread_id],
        },
        "delete_message" => Command::Trash {
            account_id,
            thread_ids: vec![thread_id],
        },
        "permanent_delete" => Command::PermanentDelete {
            account_id,
            thread_ids: vec![thread_id],
        },
        "move_message" => {
            let folder_id = custom_folder_id_arg(core, &account_id, args)?;
            Command::Move {
                account_id,
                thread_ids: vec![thread_id],
                folder_id,
            }
        }
        _ => Command::Snooze {
            account_id,
            thread_ids: vec![thread_id],
            until: args
                .get("until")
                .and_then(Value::as_i64)
                .ok_or_else(|| McpError::invalid("until is required"))?,
        },
    };
    let receipt = core.dispatch_with_receipt(command)?;
    Ok(
        json!({"queued":true,"operation_ids":receipt.operations.iter().map(|o|o.operation_id.as_str()).collect::<Vec<_>>()}),
    )
}

/// The same vector-valued Core command used by GTK's selected-thread action.
/// Validation happens before Core dispatch so malformed MCP input never
/// partially changes local flags or creates queue rows.
fn bulk_mark_read_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    require_level(access, PermissionLevel::Draft)?;
    let account_id = account_arg(args)?;
    let thread_ids = bulk_thread_ids_arg(args)?;
    let receipt = core.dispatch_with_receipt(Command::MarkRead {
        account_id,
        thread_ids,
    })?;
    Ok(
        json!({"queued":true,"operation_ids":receipt.operations.iter().map(|operation| operation.operation_id.as_str()).collect::<Vec<_>>() }),
    )
}

/// The same vector-valued Core command used by GTK's selected-thread action.
/// Validation happens before Core dispatch so malformed MCP input never
/// partially changes local flags or creates queue rows.
fn bulk_mark_unread_tool(
    core: &mut Core,
    access: &Access,
    args: &Value,
) -> Result<Value, McpError> {
    require_level(access, PermissionLevel::Draft)?;
    let account_id = account_arg(args)?;
    let thread_ids = bulk_thread_ids_arg(args)?;
    let receipt = core.dispatch_with_receipt(Command::MarkUnread {
        account_id,
        thread_ids,
    })?;
    Ok(
        json!({"queued":true,"operation_ids":receipt.operations.iter().map(|operation| operation.operation_id.as_str()).collect::<Vec<_>>() }),
    )
}

/// The same vector-valued Core command used by GTK's selected-thread Archive.
/// Validation happens before Core dispatch so malformed MCP input never
/// partially changes local placement or creates queue rows.
fn bulk_archive_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    require_level(access, PermissionLevel::Draft)?;
    let account_id = account_arg(args)?;
    let thread_ids = bulk_thread_ids_arg(args)?;
    let receipt = core.dispatch_with_receipt(Command::Archive {
        account_id,
        thread_ids,
    })?;
    Ok(
        json!({"queued":true,"operation_ids":receipt.operations.iter().map(|operation| operation.operation_id.as_str()).collect::<Vec<_>>() }),
    )
}

/// The same vector-valued Core command used by GTK's single-thread Star.
/// This is deliberately set-only, never a batch toggle: each selected thread
/// ends starred regardless of its prior local state.
fn bulk_star_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    require_level(access, PermissionLevel::Draft)?;
    let account_id = account_arg(args)?;
    let thread_ids = bulk_thread_ids_arg(args)?;
    let receipt = core.dispatch_with_receipt(Command::Star {
        account_id,
        thread_ids,
    })?;
    Ok(
        json!({"queued":true,"operation_ids":receipt.operations.iter().map(|operation| operation.operation_id.as_str()).collect::<Vec<_>>() }),
    )
}

/// The same vector-valued Core command used by GTK's single-thread Unstar.
/// This is deliberately set-only, never a batch toggle: each selected thread
/// ends unstarred regardless of its prior local state.
fn bulk_unstar_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    require_level(access, PermissionLevel::Draft)?;
    let account_id = account_arg(args)?;
    let thread_ids = bulk_thread_ids_arg(args)?;
    let receipt = core.dispatch_with_receipt(Command::Unstar {
        account_id,
        thread_ids,
    })?;
    Ok(
        json!({"queued":true,"operation_ids":receipt.operations.iter().map(|operation| operation.operation_id.as_str()).collect::<Vec<_>>() }),
    )
}

/// The same vector-valued Core command used by GTK's selected-thread Move.
/// Both the bounded ids and the custom destination are validated before Core
/// dispatch so malformed or unsafe MCP input cannot partially move threads or
/// create queue rows.
fn bulk_move_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    require_level(access, PermissionLevel::Draft)?;
    let account_id = account_arg(args)?;
    let thread_ids = bulk_thread_ids_arg(args)?;
    let folder_id = custom_folder_id_arg(core, &account_id, args)?;
    let receipt = core.dispatch_with_receipt(Command::Move {
        account_id,
        thread_ids,
        folder_id,
    })?;
    Ok(
        json!({"queued":true,"operation_ids":receipt.operations.iter().map(|operation| operation.operation_id.as_str()).collect::<Vec<_>>() }),
    )
}

/// The same vector-valued Core command used by GTK's selected-thread Snooze.
/// Validation happens before Core dispatch so malformed MCP input cannot
/// partially change local deadlines or create local undo receipts.
fn bulk_snooze_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    require_level(access, PermissionLevel::Draft)?;
    let account_id = account_arg(args)?;
    let thread_ids = bulk_thread_ids_arg(args)?;
    let until = args
        .get("until")
        .and_then(Value::as_i64)
        .ok_or_else(|| McpError::invalid("until is required"))?;
    let receipt = core.dispatch_with_receipt(Command::Snooze {
        account_id,
        thread_ids,
        until,
    })?;
    Ok(
        json!({"queued":true,"operation_ids":receipt.operations.iter().map(|operation| operation.operation_id.as_str()).collect::<Vec<_>>() }),
    )
}

/// Restore is not a generic MCP `undo(ticket)` alias: Core chooses only the
/// current thread's safe reversible Trash lifecycle and keeps the opaque
/// original operation id private.
fn restore_message_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    require_level(access, PermissionLevel::Draft)?;
    let account_id = account_arg(args)?;
    let thread_id = ThreadId(string_arg(args, "thread_id")?.into());
    match core.restore_trashed_thread(&account_id, &thread_id)? {
        UndoReceipt::Cancelled { .. } => Ok(json!({"restored":true,"queued":false})),
        UndoReceipt::ReverseQueued {
            reverse_operation_id,
            ..
        } => {
            Ok(json!({"restored":true,"queued":true,"operation_id":reverse_operation_id.as_str()}))
        }
    }
}

/// High-risk batch Trash deliberately bypasses generic authorization.  Core
/// owns the canonical batch digest and combines matching Allow-once consume
/// with this exact vector command in one transaction.
fn bulk_delete_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    if args.get("confirm").is_some() {
        return Err(McpError::invalid(
            "MCP cannot confirm this action; approve it in Feather Mail.",
        ));
    }
    let account_id = account_arg(args)?;
    require_account(access, account_id.as_str())?;
    let thread_ids = bulk_thread_ids_arg(args)?;
    match core
        .queue_mcp_bulk_trash(&access.client_id, access.ceiling, &account_id, thread_ids)
        .map_err(McpError::from)?
    {
        McpBulkHighRiskOutcome::Queued(receipt) => Ok(
            json!({"queued":true,"operation_ids":receipt.operations.iter().map(|operation| operation.operation_id.as_str()).collect::<Vec<_>>() }),
        ),
        McpBulkHighRiskOutcome::NeedsConfirmation(request) => {
            Err(McpError::confirmation(request.id))
        }
        McpBulkHighRiskOutcome::Denied => Err(McpError::permission()),
    }
}

/// Permanent deletion has the same exact-batch approval boundary as Trash,
/// but Core receives the distinct irreversible command and capability.
fn bulk_permanent_delete_tool(
    core: &mut Core,
    access: &Access,
    args: &Value,
) -> Result<Value, McpError> {
    if args.get("confirm").is_some() {
        return Err(McpError::invalid(
            "MCP cannot confirm this action; approve it in Feather Mail.",
        ));
    }
    let account_id = account_arg(args)?;
    require_account(access, account_id.as_str())?;
    let thread_ids = bulk_thread_ids_arg(args)?;
    match core
        .queue_mcp_bulk_permanent_delete(&access.client_id, access.ceiling, &account_id, thread_ids)
        .map_err(McpError::from)?
    {
        McpBulkHighRiskOutcome::Queued(receipt) => Ok(
            json!({"queued":true,"operation_ids":receipt.operations.iter().map(|operation| operation.operation_id.as_str()).collect::<Vec<_>>() }),
        ),
        McpBulkHighRiskOutcome::NeedsConfirmation(request) => {
            Err(McpError::confirmation(request.id))
        }
        McpBulkHighRiskOutcome::Denied => Err(McpError::permission()),
    }
}

/// T-060u. Like the two bulk doors above, this bypasses generic
/// authorization: Core owns the approval fingerprint and consumes it in the
/// same transaction as the deletion.
///
/// It is not routed through `custom_folder_id_arg` the way `move_message`
/// is. That helper reads `list_folders`, which hides tombstoned folders --
/// so a folder deleted a second time would come back as "unknown folder"
/// rather than as the plain no-op Core reports. Core re-checks the folder's
/// kind anyway, inside the transaction, which is the only place the check
/// cannot go stale.
fn delete_folder_tool(core: &mut Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    if args.get("confirm").is_some() {
        return Err(McpError::invalid(
            "MCP cannot confirm this action; approve it in Feather Mail.",
        ));
    }
    let account_id = account_arg(args)?;
    require_account(access, account_id.as_str())?;
    let folder_id = FolderId(string_arg(args, "folder_id")?.into());
    match core
        .queue_mcp_delete_folder(&access.client_id, access.ceiling, &account_id, &folder_id)
        .map_err(McpError::from)?
    {
        // `queued` false means the folder had never reached the server, so
        // there was no mailbox to delete -- the folder is gone either way.
        McpFolderDeleteOutcome::Deleted { queued } => {
            Ok(json!({"folder_id":folder_id.as_str(),"deleted":true,"queued":queued}))
        }
        McpFolderDeleteOutcome::NeedsConfirmation(request) => {
            Err(McpError::confirmation(request.id))
        }
        McpFolderDeleteOutcome::Denied => Err(McpError::permission()),
    }
}

/// `FolderSummary` is the public Core boundary for the same local custom
/// destination policy used by the single-thread MCP Move action.
fn custom_folder_id_arg(
    core: &Core,
    account_id: &AccountId,
    args: &Value,
) -> Result<FolderId, McpError> {
    let folder_id = FolderId(string_arg(args, "folder_id")?.into());
    if core
        .list_folders(account_id)?
        .iter()
        .any(|folder| folder.folder.id == folder_id && folder.folder.kind == FolderKind::Custom)
    {
        Ok(folder_id)
    } else {
        Err(McpError::invalid(
            "folder_id does not name a custom folder in this account.",
        ))
    }
}

fn bulk_thread_ids_arg(args: &Value) -> Result<Vec<ThreadId>, McpError> {
    let ids = args
        .get("thread_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::invalid("thread_ids must be a non-empty array."))?;
    if ids.is_empty() || ids.len() > MAX_BULK_THREAD_IDS {
        return Err(McpError::invalid(
            "thread_ids must contain between 1 and 100 values.",
        ));
    }
    let mut unique = HashSet::with_capacity(ids.len());
    let mut thread_ids = Vec::with_capacity(ids.len());
    for id in ids {
        let id = id
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| McpError::invalid("thread_ids must contain non-empty strings."))?;
        if !unique.insert(id) {
            return Err(McpError::invalid(
                "thread_ids must not contain duplicate values.",
            ));
        }
        thread_ids.push(ThreadId(id.into()));
    }
    Ok(thread_ids)
}

/// The same 1…100 unique non-empty bound as the thread batches, applied to
/// message ids. Parsing happens before any Core read so a malformed array
/// never reaches the store and never appears in an error or audit row.
fn bulk_message_ids_arg(args: &Value) -> Result<Vec<MessageId>, McpError> {
    let ids = args
        .get("message_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::invalid("message_ids must be a non-empty array."))?;
    if ids.is_empty() || ids.len() > MAX_BULK_THREAD_IDS {
        return Err(McpError::invalid(
            "message_ids must contain between 1 and 100 values.",
        ));
    }
    let mut unique = HashSet::with_capacity(ids.len());
    let mut message_ids = Vec::with_capacity(ids.len());
    for id in ids {
        let id = id
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| McpError::invalid("message_ids must contain non-empty strings."))?;
        if !unique.insert(id) {
            return Err(McpError::invalid(
                "message_ids must not contain duplicate values.",
            ));
        }
        message_ids.push(MessageId(id.into()));
    }
    Ok(message_ids)
}

fn save_draft_tool(core: &Core, access: &Access, args: &Value) -> Result<Value, McpError> {
    require_level(access, PermissionLevel::Draft)?;
    let draft_id = args
        .get("draft_id")
        .and_then(Value::as_str)
        .map(|s| DraftId(s.into()));
    let content = DraftContent {
        thread_id: args
            .get("thread_id")
            .and_then(Value::as_str)
            .map(|s| ThreadId(s.into())),
        in_reply_to: args
            .get("in_reply_to")
            .and_then(Value::as_str)
            .map(|s| MessageId(s.into())),
        from: string_arg(args, "from")?.into(),
        to: optional_string(args, "to"),
        cc: optional_string(args, "cc"),
        bcc: optional_string(args, "bcc"),
        subject: optional_string(args, "subject"),
        body: optional_string(args, "body"),
    };
    Ok(draft_metadata_json(&core.save_draft(
        &account_arg(args)?,
        draft_id.as_ref(),
        content,
    )?))
}

fn require_level(access: &Access, level: PermissionLevel) -> Result<(), McpError> {
    if access.ceiling < level {
        Err(McpError::permission())
    } else {
        Ok(())
    }
}
fn require_account(access: &Access, account: &str) -> Result<(), McpError> {
    if !access.accounts.is_empty() && !access.accounts.contains(account) {
        Err(McpError::permission())
    } else {
        Ok(())
    }
}
fn account_arg(args: &Value) -> Result<AccountId, McpError> {
    Ok(AccountId(string_arg(args, "account_id")?.into()))
}
fn string_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, McpError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| McpError::invalid(&format!("{key} is required")))
}
fn optional_string(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .into()
}
fn limit_arg(args: &Value) -> Result<usize, McpError> {
    match args.get("limit") {
        None => Ok(50),
        Some(value) => {
            let Some(n) = value.as_u64() else {
                return Err(McpError::invalid(
                    "limit must be an integer between 1 and 200.",
                ));
            };
            if !(1..=200).contains(&n) {
                return Err(McpError::invalid(
                    "limit must be an integer between 1 and 200.",
                ));
            }
            Ok(n as usize)
        }
    }
}

/// Parse the sole cursor shape Core accepts for both local thread and search
/// continuation. It is a structured value, never an opaque or encoded token.
fn cursor_arg(args: &Value) -> Result<Option<ThreadCursor>, McpError> {
    let Some(raw) = args.get("after") else {
        return Ok(None);
    };
    let object = raw
        .as_object()
        .ok_or_else(|| McpError::invalid("after must be a cursor object."))?;
    if object.len() != 2 || !object.contains_key("date") || !object.contains_key("id") {
        return Err(McpError::invalid("after must contain exactly date and id."));
    }
    let date = object
        .get("date")
        .and_then(Value::as_i64)
        .ok_or_else(|| McpError::invalid("after.date must be an integer."))?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| McpError::invalid("after.id must be a non-empty string."))?;
    Ok(Some(ThreadCursor {
        date,
        id: ThreadId(id.into()),
    }))
}

fn cursor_json(cursor: &ThreadCursor) -> Value {
    json!({"date":cursor.date,"id":cursor.id.as_str()})
}

/// One folder row. `list_folders` and `get_folder` share this projection so
/// the single-folder read can never drift into a second folder shape. Nothing
/// here is provider state: `create_failed` is the local Core flag the sidebar
/// already renders, and no remote mailbox path is exposed.
fn folder_json(summary: &feathermail_core::FolderSummary) -> Value {
    json!({"id":summary.folder.id.as_str(),"name":summary.folder.label,"kind":summary.folder.kind.as_str(),"unread":summary.unread,"total":summary.total,"create_failed":summary.folder.create_failed})
}

fn thread_json(t: &Thread) -> Value {
    json!({"id":t.id.as_str(),"account_id":t.account_id.as_str(),"folder_id":t.folder.as_str(),"from":{"name":t.from.name,"email":t.from.email},"subject":t.subject,"date":t.date,"unread":t.unread(),"starred":t.starred,"has_attachment":t.has_attachment})
}

fn thread_message_json(message: &feathermail_core::ThreadMessage) -> Value {
    json!({"id":message.id.as_str(),"date":message.date,"from":{"name":message.from.name,"email":message.from.email},"subject":message.subject,"unread":message.unread,"starred":message.starred,"has_attachment":message.has_attachment})
}

/// The single-message endpoint adds only local navigation identity to the
/// existing thread-message header/state projection. Provider coordinates, raw
/// headers, size/cache information, preview and every body stay in Core.
fn message_metadata_json(message: &feathermail_core::ThreadMessage) -> Value {
    json!({"id":message.id.as_str(),"thread_id":message.thread_id.as_str(),"folder_id":message.folder.as_str(),"date":message.date,"from":{"name":message.from.name,"email":message.from.email},"subject":message.subject,"unread":message.unread,"starred":message.starred,"has_attachment":message.has_attachment})
}
/// MCP results deliberately project editable drafts to headers and identity.
/// Draft body text stays in Core and is accepted only as create/update input.
fn draft_metadata_json(d: &feathermail_core::Draft) -> Value {
    json!({"id":d.id.as_str(),"account_id":d.account_id.as_str(),"thread_id":d.thread_id.as_ref().map(ThreadId::as_str),"in_reply_to":d.in_reply_to.as_ref().map(MessageId::as_str),"from":d.from,"to":d.to,"cc":d.cc,"bcc":d.bcc,"subject":d.subject,"updated_at":d.updated_at})
}

fn attachment_json(attachment: &Attachment, cached: bool) -> Value {
    json!({
        "id": attachment.id.as_str(),
        "account_id": attachment.account_id.as_str(),
        "message_id": attachment.message_id.as_str(),
        "filename": attachment.filename,
        "mime": attachment.mime,
        "size_bytes": attachment.size_bytes,
        "cached": cached,
    })
}

/// A cache pointer is private implementation state. It becomes a real source
/// only after resolving it under the one cache root and rejecting symlinks or
/// traversal that would leave it. A stale row is indistinguishable from a
/// cache miss to MCP, rather than leaking the actual profile path.
fn cached_attachment_path_in(
    attachment: &Attachment,
    cache_root: &Path,
) -> Result<PathBuf, McpError> {
    let relative = attachment.cache_path.as_ref().ok_or_else(not_cached)?;
    if relative.is_absolute()
        || relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(not_cached());
    }
    let root = cache_root.canonicalize().map_err(|_| not_cached())?;
    let source = root
        .join(relative)
        .canonicalize()
        .map_err(|_| not_cached())?;
    if !source.starts_with(&root) || !source.is_file() {
        return Err(not_cached());
    }
    Ok(source)
}

fn attachment_is_cached(attachment: &Attachment) -> bool {
    cached_attachment_path_in(attachment, &default_attachments_dir()).is_ok()
}

fn not_cached() -> McpError {
    CoreError::new(
        ErrorCode::OperationNotSupported,
        "This attachment has not been downloaded locally yet.",
    )
    .into()
}

fn attachment_root(access: &Access) -> Result<PathBuf, McpError> {
    let root = access
        .attachment_root
        .as_ref()
        .ok_or_else(McpError::permission)?
        .canonicalize()
        .map_err(|_| McpError::permission())?;
    if !root.is_dir() {
        return Err(McpError::permission());
    }
    Ok(root)
}

/// Resolve a new export target below the configured root. Existing files and
/// `.part` siblings are rejected: an MCP Read action must never overwrite a
/// local file, and `stream_to_file` therefore gets a fresh atomic target.
fn safe_attachment_destination(root: &Path, raw: &Path) -> Result<PathBuf, McpError> {
    if !raw.is_absolute() {
        return Err(McpError::invalid(
            "Attachment destination must be an absolute path.",
        ));
    }
    let parent = raw
        .parent()
        .ok_or_else(|| McpError::invalid("Attachment destination is not valid."))?
        .canonicalize()
        .map_err(|_| McpError::invalid("Attachment destination is not available."))?;
    let filename = raw
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| McpError::invalid("Attachment destination is not valid."))?;
    if !parent.starts_with(root) {
        return Err(McpError::permission());
    }
    let destination = parent.join(filename);
    let mut partial = filename.to_os_string();
    partial.push(".part");
    if fs::symlink_metadata(&destination).is_ok()
        || fs::symlink_metadata(parent.join(partial)).is_ok()
    {
        return Err(McpError::invalid(
            "Attachment destination already exists; choose a new filename.",
        ));
    }
    Ok(destination)
}

fn safe_attachment_filename(filename: &str) -> PathBuf {
    const MAX_FILENAME_BYTES: usize = 180;
    let mut safe = String::new();
    for character in filename.trim().chars() {
        let character = if character == '/' || character == '\\' || character.is_control() {
            '_'
        } else {
            character
        };
        if safe.len() + character.len_utf8() > MAX_FILENAME_BYTES {
            break;
        }
        safe.push(character);
    }
    if safe.is_empty() || safe == "." || safe == ".." {
        PathBuf::from("attachment")
    } else {
        PathBuf::from(safe)
    }
}

fn export_cached_attachment(
    attachment: &Attachment,
    cache_root: &Path,
    destination: &Path,
) -> Result<(), McpError> {
    let source = cached_attachment_path_in(attachment, cache_root)?;
    let source = fs::File::open(source).map_err(|_| not_cached())?;
    stream_to_file(source, destination, None).map_err(|_| {
        McpError::from(CoreError::new(
            ErrorCode::OperationNotSupported,
            "Could not save this attachment to that destination.",
        ))
    })?;
    Ok(())
}

fn safe_attachment_path(access: &Access, raw: &str) -> Result<PathBuf, McpError> {
    let root = attachment_root(access)?;
    let path = Path::new(raw)
        .canonicalize()
        .map_err(|_| McpError::invalid("Attachment file is not available."))?;
    if !path.starts_with(root) || !path.is_file() {
        return Err(McpError::permission());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use feathermail_core::{
        Address, ApplyError, ConnectError, ConnectOk, Draft, FolderKind, Importance, MailConnector,
        MailProvider, MailSecurity, MailboxForm, McpConfirmationChoice, Operation, OperationId,
        Placement, TickOutcome, UndoTicket,
    };
    use feathermail_sync::{HeaderMeta, SyncStore};

    struct LocalProbe;

    impl MailConnector for LocalProbe {
        fn probe(&self, _form: &MailboxForm, _password: &str) -> Result<ConnectOk, ConnectError> {
            Ok(ConnectOk {
                capabilities: Vec::new(),
            })
        }
    }

    struct AcceptProvider;

    impl MailProvider for AcceptProvider {
        fn apply(&mut self, _op: &Operation) -> Result<(), ApplyError> {
            Ok(())
        }
    }

    fn seeded_cursor_core() -> (Core, AccountId, FolderId) {
        let mut core = Core::memory().unwrap();
        core.set_mcp_enabled(1, true).unwrap();
        // High-risk MCP cases below model the user deliberately selecting
        // Full in Settings. A process Full ceiling alone must never elevate
        // the fresh durable Draft policy.
        assert!(core
            .set_mcp_client_permission_level("stdio", PermissionLevel::Full)
            .unwrap());
        let form = MailboxForm {
            email: "cursor@example.test".into(),
            imap_host: "imap.example.test".into(),
            imap_port: 993,
            imap_security: MailSecurity::Ssl,
            smtp_host: "smtp.example.test".into(),
            smtp_port: 465,
            smtp_security: MailSecurity::Ssl,
        };
        let probe_input = "x".to_string();
        let account = core.add_account(&form, &probe_input, &LocalProbe).unwrap();
        let inbox = core
            .list_folders(&account)
            .unwrap()
            .into_iter()
            .find(|summary| summary.folder.kind == FolderKind::Inbox)
            .unwrap()
            .folder
            .id;
        let headers = (1..=4)
            .map(|uid| HeaderMeta {
                uid,
                flags: vec![],
                size_bytes: Some(100),
                message_id: Some(format!("<cursor-{uid}@example.test>")),
                from: Some("Cursor sender <sender@example.test>".into()),
                to: Some("cursor@example.test".into()),
                subject: Some(format!("cursor page {uid}")),
                date: Some(format!("0{uid} Jan 2024 00:00:00 +0000")),
                ..HeaderMeta::default()
            })
            .collect::<Vec<_>>();
        core.sync_store(&account, inbox.as_str())
            .upsert_headers("INBOX", &headers)
            .unwrap();
        let bodies_dir = tempfile::tempdir().unwrap();
        let indexed = core.index_pending_batch(bodies_dir.path(), 10).unwrap();
        assert_eq!(indexed.indexed, 4);
        assert_eq!(indexed.remaining, 0);
        (core, account, inbox)
    }

    fn thread_ids(page: &Value) -> Vec<String> {
        page["threads"]
            .as_array()
            .unwrap()
            .iter()
            .map(|thread| thread["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn assert_thread_metadata_only(page: &Value) {
        for thread in page["threads"].as_array().unwrap() {
            assert!(thread.get("body").is_none());
            assert!(thread.get("preview").is_none());
        }
    }

    fn assert_no_submitted_sensitive_markers(value: &Value) {
        let rendered = serde_json::to_string(value).unwrap();
        for marker in [
            "mcp-test-body-marker",
            "mcp-test-password-marker",
            "mcp-test-token-marker",
        ] {
            assert!(
                !rendered.contains(marker),
                "MCP response leaked submitted marker: {marker}"
            );
        }
    }

    fn assert_mcp_cursor_continuation(
        core: &mut Core,
        tool: &str,
        first_args: Value,
        expected_ids: Vec<String>,
    ) {
        let first = call_tool(core, &Access::default(), tool, &first_args).unwrap();
        assert_eq!(thread_ids(&first).len(), 2);
        assert_thread_metadata_only(&first);
        let after = first["next"].clone();
        assert!(after.is_object());

        let mut second_args = first_args;
        second_args
            .as_object_mut()
            .unwrap()
            .insert("after".into(), after.clone());
        assert_eq!(second_args["after"], first["next"]);
        let second = call_tool(core, &Access::default(), tool, &second_args).unwrap();
        assert_thread_metadata_only(&second);
        assert_eq!(second["next"], Value::Null);

        let mut ids = thread_ids(&first);
        ids.extend(thread_ids(&second));
        assert_eq!(ids, expected_ids);
    }

    fn incoming_attachment(cache_path: Option<PathBuf>) -> Attachment {
        Attachment {
            id: AttachmentId("attachment:m1:0".into()),
            account_id: AccountId("acc1".into()),
            message_id: MessageId("m1".into()),
            filename: "report.pdf".into(),
            mime: "application/pdf".into(),
            size_bytes: 3,
            cache_path,
            content_id: None,
            part_path: Some("2".into()),
            transfer_encoding: feathermail_core::AttachmentEncoding::Base64,
        }
    }

    #[test]
    fn tools_have_object_schemas_and_unique_names() {
        let tools = tool_definitions();
        let mut names = HashSet::new();
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(names.insert(tool["name"].as_str().unwrap().to_string()));
        }
    }

    #[test]
    fn thread_list_tools_share_the_typed_core_cursor_schema() {
        let expected = cursor_schema();
        for name in ["list_threads", "list_snoozed", "search_mail"] {
            let tool = tool_definitions()
                .into_iter()
                .find(|tool| tool["name"] == name)
                .unwrap();
            assert_eq!(tool["inputSchema"]["properties"]["after"], expected);
        }
    }

    #[test]
    fn list_tools_reject_a_limit_outside_the_declared_schema() {
        // T-116: the schema says 1..=200. Clamping 0 to 1, or treating
        // -5 / 1.7 / "abc" as the default, is a silent rewrite — bulk
        // tools already answer INVALID_ARGUMENT for the same class of
        // input.
        for value in [json!(0), json!(-5), json!(1.7), json!("abc"), json!(201)] {
            let error = limit_arg(&json!({"limit": value})).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert!(error.message.contains("1 and 200"));
        }
        assert_eq!(limit_arg(&json!({})).unwrap(), 50);
        assert_eq!(limit_arg(&json!({"limit": 1})).unwrap(), 1);
        assert_eq!(limit_arg(&json!({"limit": 200})).unwrap(), 200);

        let (mut core, account, inbox) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access::default(),
            "list_threads",
            &json!({
                "account_id": account.as_str(),
                "folder_id": inbox.as_str(),
                "limit": 0
            }),
        )
        .unwrap_err();
        assert_eq!(error.code, "INVALID_ARGUMENT");
    }

    #[test]
    fn bulk_thread_schemas_require_a_bounded_unique_non_empty_id_list() {
        for name in [
            "bulk_mark_read",
            "bulk_mark_unread",
            "bulk_archive",
            "bulk_star",
            "bulk_delete",
            "bulk_permanent_delete",
        ] {
            let tool = tool_definitions()
                .into_iter()
                .find(|tool| tool["name"] == name)
                .unwrap();
            let schema = &tool["inputSchema"];
            assert_eq!(schema["required"], json!(["account_id", "thread_ids"]));
            assert_eq!(schema["additionalProperties"], false);
            assert_eq!(schema["properties"]["thread_ids"]["minItems"], 1);
            assert_eq!(
                schema["properties"]["thread_ids"]["maxItems"],
                MAX_BULK_THREAD_IDS
            );
            assert_eq!(schema["properties"]["thread_ids"]["uniqueItems"], true);
            assert_eq!(schema["properties"]["thread_ids"]["items"]["minLength"], 1);
        }

        let tool = tool_definitions()
            .into_iter()
            .find(|tool| tool["name"] == "bulk_move")
            .unwrap();
        let schema = &tool["inputSchema"];
        assert_eq!(
            schema["required"],
            json!(["account_id", "folder_id", "thread_ids"])
        );
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["folder_id"]["type"], "string");
        assert_eq!(schema["properties"]["thread_ids"]["minItems"], 1);
        assert_eq!(
            schema["properties"]["thread_ids"]["maxItems"],
            MAX_BULK_THREAD_IDS
        );
        assert_eq!(schema["properties"]["thread_ids"]["uniqueItems"], true);
        assert_eq!(schema["properties"]["thread_ids"]["items"]["minLength"], 1);

        let tool = tool_definitions()
            .into_iter()
            .find(|tool| tool["name"] == "bulk_snooze")
            .unwrap();
        let schema = &tool["inputSchema"];
        assert_eq!(
            schema["required"],
            json!(["account_id", "thread_ids", "until"])
        );
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["until"]["type"], "integer");
        assert_eq!(schema["properties"]["thread_ids"]["minItems"], 1);
        assert_eq!(
            schema["properties"]["thread_ids"]["maxItems"],
            MAX_BULK_THREAD_IDS
        );
        assert_eq!(schema["properties"]["thread_ids"]["uniqueItems"], true);
        assert_eq!(schema["properties"]["thread_ids"]["items"]["minLength"], 1);
    }

    #[test]
    fn mcp_cursor_round_trips_the_core_thread_cursor_shape() {
        let cursor = cursor_arg(&json!({"after":{"date":17,"id":"thread-17"}}))
            .unwrap()
            .unwrap();
        assert_eq!(
            cursor,
            ThreadCursor {
                date: 17,
                id: ThreadId("thread-17".into()),
            }
        );
        assert_eq!(cursor_json(&cursor), json!({"date":17,"id":"thread-17"}));
        assert!(cursor_arg(&json!({})).unwrap().is_none());
        for after in [
            json!(null),
            json!({"date":17}),
            json!({"date":"17","id":"thread-17"}),
            json!({"date":17,"id":"","extra":true}),
        ] {
            let error = cursor_arg(&json!({"after":after})).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
        }
    }

    #[test]
    fn list_threads_continues_the_seeded_core_page_without_overlap_or_skip() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let expected_ids = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 4,
            })
            .unwrap()
            .threads
            .iter()
            .map(|thread| thread.id.as_str().to_string())
            .collect();

        assert_mcp_cursor_continuation(
            &mut core,
            "list_threads",
            json!({"account_id":account.as_str(),"folder_id":inbox.as_str(),"limit":2}),
            expected_ids,
        );
    }

    #[test]
    fn list_snoozed_continues_the_core_virtual_folder_without_overlap_or_skip() {
        let (mut core, account, inbox) = seeded_cursor_core();
        core.set_now(1_000);
        let snoozed_ids = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads
            .into_iter()
            .map(|thread| thread.id)
            .collect();
        core.dispatch(Command::Snooze {
            account_id: account.clone(),
            thread_ids: snoozed_ids,
            until: 2_000,
        })
        .unwrap();
        let expected_ids: Vec<_> = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: FolderId("snoozed".into()),
                filter: ThreadFilter::All,
                after: None,
                limit: 4,
            })
            .unwrap()
            .threads
            .iter()
            .map(|thread| thread.id.as_str().to_string())
            .collect();
        assert_eq!(expected_ids.len(), 3);

        assert_mcp_cursor_continuation(
            &mut core,
            "list_snoozed",
            json!({"account_id":account.as_str(),"limit":2}),
            expected_ids,
        );
    }

    #[test]
    fn list_snoozed_refuses_an_account_outside_the_ceiling() {
        let (mut core, account, _) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
            "list_snoozed",
            &json!({"account_id":account.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
    }

    #[test]
    fn folder_message_count_matches_core_folder_summaries_for_real_and_virtual_folders() {
        let (mut core, account, inbox) = seeded_cursor_core();
        core.set_now(1_000);
        let thread_id = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap()
            .id;
        core.dispatch(Command::Snooze {
            account_id: account.clone(),
            thread_ids: vec![thread_id],
            until: 2_000,
        })
        .unwrap();

        for folder_id in [inbox.as_str(), "snoozed"] {
            let expected = core
                .list_folders(&account)
                .unwrap()
                .into_iter()
                .find(|folder| folder.folder.id.as_str() == folder_id)
                .unwrap();
            let response = call_tool(
                &mut core,
                &Access {
                    ceiling: PermissionLevel::Read,
                    ..Access::default()
                },
                "get_folder_message_count",
                &json!({"account_id":account.as_str(),"folder_id":folder_id}),
            )
            .unwrap();
            assert_eq!(
                response,
                json!({"folder_id":folder_id,"unread":expected.unread,"total":expected.total}),
            );
        }
    }

    /// T-060s: the tool is a durable request, not an IMAP call. Repeating
    /// it while the window has not yet claimed anything must still leave
    /// exactly one pending sync -- an agent in a retry loop must not turn
    /// into a queue of wakes.
    #[test]
    fn sync_account_records_one_collapsed_pending_request() {
        let (mut core, account, _) = seeded_cursor_core();
        let access = Access {
            ceiling: PermissionLevel::Draft,
            ..Access::default()
        };
        for _ in 0..3 {
            let response = call_tool(
                &mut core,
                &access,
                "sync_account",
                &json!({"account_id":account.as_str()}),
            )
            .unwrap();
            assert_eq!(response["queued"], json!(true));
            assert_eq!(response["account_id"], json!(account.as_str()));
        }
        let claimed = core.take_sync_requests(8).unwrap();
        assert_eq!(
            claimed.iter().map(AccountId::as_str).collect::<Vec<_>>(),
            vec![account.as_str()],
            "three calls, one sync"
        );
        assert!(
            core.take_sync_requests(8).unwrap().is_empty(),
            "the claim consumed it"
        );
    }

    #[test]
    fn sync_account_rejects_an_account_outside_the_ceiling() {
        let (mut core, account, _) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Draft,
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
            "sync_account",
            &json!({"account_id":account.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
        assert!(
            core.take_sync_requests(8).unwrap().is_empty(),
            "a denied call leaves no request behind"
        );
    }

    /// A Read-only client can look at mail; it cannot make the app reach
    /// out to the server.
    #[test]
    fn sync_account_refuses_a_read_only_client() {
        let (mut core, account, _) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            "sync_account",
            &json!({"account_id":account.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
        assert!(core.take_sync_requests(8).unwrap().is_empty());
    }

    #[test]
    fn sync_account_rejects_an_unknown_account() {
        let (mut core, _, _) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Draft,
                ..Access::default()
            },
            "sync_account",
            &json!({"account_id":"nobody"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "ACCOUNT_NOT_FOUND");
    }

    #[test]
    fn get_folder_projects_exactly_one_list_folders_row() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let listed = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            "list_folders",
            &json!({"account_id":account.as_str()}),
        )
        .unwrap();
        let expected = listed["folders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|folder| folder["id"] == json!(inbox.as_str()))
            .unwrap()
            .clone();

        let response = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            "get_folder",
            &json!({"account_id":account.as_str(),"folder_id":inbox.as_str()}),
        )
        .unwrap();

        // One projection, not a second folder shape: the single read is
        // byte-identical to the row the list already returns.
        assert_eq!(response, expected);
        // No provider coordinate or remote mailbox path leaves Core.
        assert!(response.get("path").is_none());
        assert!(response.get("remote").is_none());
        assert!(response.get("uid_validity").is_none());
    }

    #[test]
    fn get_folder_rejects_a_folder_that_is_not_in_this_account() {
        let (mut core, account, _) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            "get_folder",
            &json!({"account_id":account.as_str(),"folder_id":"no-such-folder"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "INVALID_ARGUMENT");
        assert!(!error.message.contains("no-such-folder"));
    }

    #[test]
    fn get_folder_refuses_an_account_outside_the_ceiling() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
            "get_folder",
            &json!({"account_id":account.as_str(),"folder_id":inbox.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
    }

    #[test]
    fn folder_message_count_refuses_an_account_outside_the_ceiling() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
            "get_folder_message_count",
            &json!({"account_id":account.as_str(),"folder_id":inbox.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
    }

    #[test]
    fn account_status_matches_the_current_core_account_row_without_metadata() {
        let (mut core, account, _) = seeded_cursor_core();
        let expected = core
            .list_accounts()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == account)
            .unwrap();
        let response = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            "get_account_status",
            &json!({"account_id":account.as_str()}),
        )
        .unwrap();
        assert_eq!(
            response,
            json!({"account_id":account.as_str(),"status":expected.status.as_str()}),
        );
        assert!(response.get("name").is_none());
        assert!(response.get("email").is_none());
    }

    #[test]
    fn account_status_reports_an_unknown_account_without_syncing() {
        let (mut core, _, _) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access::default(),
            "get_account_status",
            &json!({"account_id":"missing-account"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "ACCOUNT_NOT_FOUND");
    }

    #[test]
    fn account_status_refuses_an_account_outside_the_ceiling() {
        let (mut core, account, _) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
            "get_account_status",
            &json!({"account_id":account.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
    }

    #[test]
    fn bulk_mark_read_uses_the_existing_core_vector_command_and_returns_queue_metadata_only() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads;
        assert!(
            threads[..2].iter().all(Thread::unread),
            "the seeded targets must prove that the batch changed their flags"
        );
        let selected = threads[..2]
            .iter()
            .map(|thread| thread.id.as_str().to_string())
            .collect::<Vec<_>>();
        let untouched = threads[2].id.clone();
        let untouched_before = core.get_thread(&account, &untouched).unwrap().unread();
        let pending_before = core.queue_counts().unwrap().pending;

        let response = call_tool(
            &mut core,
            &Access::default(),
            "bulk_mark_read",
            &json!({"account_id":account.as_str(),"thread_ids":selected}),
        )
        .unwrap();

        assert_eq!(response["queued"], true);
        assert_eq!(response.as_object().unwrap().len(), 2);
        assert!(response.get("thread_ids").is_none());
        let operation_ids = response["operation_ids"].as_array().unwrap();
        assert_eq!(operation_ids.len(), 2);
        assert_eq!(
            operation_ids
                .iter()
                .map(|id| id.as_str().unwrap())
                .collect::<HashSet<_>>()
                .len(),
            2
        );
        assert_eq!(core.queue_counts().unwrap().pending, pending_before + 2);
        for thread_id in selected {
            assert!(!core
                .get_thread(&account, &ThreadId(thread_id))
                .unwrap()
                .unread());
        }
        assert_eq!(
            core.get_thread(&account, &untouched).unwrap().unread(),
            untouched_before
        );
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "bulk_mark_read");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn bulk_mark_read_fails_closed_for_an_insufficient_or_foreign_ceiling_without_queueing() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let args = json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()]});
        let pending_before = core.queue_counts().unwrap().pending;
        let unread_before = core.get_thread(&account, &thread.id).unwrap().unread();

        for access in [
            Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "bulk_mark_read", &args).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(
                core.get_thread(&account, &thread.id).unwrap().unread(),
                unread_before
            );
        }
    }

    #[test]
    fn bulk_mark_read_rejects_malformed_or_unknown_batches_atomically_without_disclosing_ids() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let pending_before = core.queue_counts().unwrap().pending;
        let unread_before = core.get_thread(&account, &thread.id).unwrap().unread();
        let over_limit = Value::Array(
            (0..=MAX_BULK_THREAD_IDS)
                .map(|index| Value::String(format!("batch-{index}")))
                .collect(),
        );

        for args in [
            json!({"account_id":account.as_str()}),
            json!({"account_id":account.as_str(),"thread_ids":[]}),
            json!({"account_id":account.as_str(),"thread_ids":[""]}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),thread.id.as_str()]}),
            json!({"account_id":account.as_str(),"thread_ids":over_limit}),
        ] {
            let error =
                call_tool(&mut core, &Access::default(), "bulk_mark_read", &args).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(
                core.get_thread(&account, &thread.id).unwrap().unread(),
                unread_before
            );
        }

        let unknown_id = "unknown-bulk-thread";
        let error = call_tool(
            &mut core,
            &Access::default(),
            "bulk_mark_read",
            &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown_id]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
        assert!(!error.message.contains(unknown_id));
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert_eq!(
            core.get_thread(&account, &thread.id).unwrap().unread(),
            unread_before
        );
        assert!(
            !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(unknown_id),
            "unknown submitted ids must never reach the metadata-only audit"
        );
    }

    #[test]
    fn bulk_mark_unread_uses_the_existing_core_vector_command_and_returns_queue_metadata_only() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads;
        let selected = threads[..2]
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>();
        assert!(
            selected
                .iter()
                .all(|thread_id| core.get_thread(&account, thread_id).unwrap().unread()),
            "the seeded targets must become read before the unread batch"
        );
        core.dispatch(Command::MarkRead {
            account_id: account.clone(),
            thread_ids: selected.clone(),
        })
        .unwrap();
        assert!(selected
            .iter()
            .all(|thread_id| !core.get_thread(&account, thread_id).unwrap().unread()));
        let untouched = threads[2].id.clone();
        let untouched_before = core.get_thread(&account, &untouched).unwrap().unread();
        let pending_before = core.queue_counts().unwrap().pending;

        let response = call_tool(
            &mut core,
            &Access::default(),
            "bulk_mark_unread",
            &json!({"account_id":account.as_str(),"thread_ids":selected.iter().map(ThreadId::as_str).collect::<Vec<_>>() }),
        )
        .unwrap();

        assert_eq!(response["queued"], true);
        assert_eq!(response.as_object().unwrap().len(), 2);
        assert!(response.get("thread_ids").is_none());
        let operation_ids = response["operation_ids"].as_array().unwrap();
        assert_eq!(operation_ids.len(), 2);
        assert_eq!(
            operation_ids
                .iter()
                .map(|id| id.as_str().unwrap())
                .collect::<HashSet<_>>()
                .len(),
            2
        );
        assert_eq!(core.queue_counts().unwrap().pending, pending_before + 2);
        assert!(selected
            .iter()
            .all(|thread_id| core.get_thread(&account, thread_id).unwrap().unread()));
        assert_eq!(
            core.get_thread(&account, &untouched).unwrap().unread(),
            untouched_before
        );
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "bulk_mark_unread");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn bulk_mark_unread_fails_closed_for_an_insufficient_or_foreign_ceiling_without_queueing() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let args = json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()]});
        let pending_before = core.queue_counts().unwrap().pending;
        let unread_before = core.get_thread(&account, &thread.id).unwrap().unread();

        for access in [
            Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "bulk_mark_unread", &args).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(
                core.get_thread(&account, &thread.id).unwrap().unread(),
                unread_before
            );
        }
    }

    #[test]
    fn bulk_mark_unread_rejects_malformed_or_unknown_batches_atomically_without_disclosing_ids() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        core.dispatch(Command::MarkRead {
            account_id: account.clone(),
            thread_ids: vec![thread.id.clone()],
        })
        .unwrap();
        let pending_before = core.queue_counts().unwrap().pending;
        let unread_before = core.get_thread(&account, &thread.id).unwrap().unread();
        assert!(!unread_before);
        let over_limit = Value::Array(
            (0..=MAX_BULK_THREAD_IDS)
                .map(|index| Value::String(format!("batch-{index}")))
                .collect(),
        );

        for args in [
            json!({"account_id":account.as_str()}),
            json!({"account_id":account.as_str(),"thread_ids":[]}),
            json!({"account_id":account.as_str(),"thread_ids":[""]}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),thread.id.as_str()]}),
            json!({"account_id":account.as_str(),"thread_ids":over_limit}),
        ] {
            let error =
                call_tool(&mut core, &Access::default(), "bulk_mark_unread", &args).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(
                core.get_thread(&account, &thread.id).unwrap().unread(),
                unread_before
            );
        }

        let unknown_id = "unknown-bulk-unread-thread";
        let error = call_tool(
            &mut core,
            &Access::default(),
            "bulk_mark_unread",
            &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown_id]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
        assert!(!error.message.contains(unknown_id));
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert_eq!(
            core.get_thread(&account, &thread.id).unwrap().unread(),
            unread_before
        );
        assert!(
            !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(unknown_id),
            "unknown submitted ids must never reach the metadata-only audit"
        );
    }

    #[test]
    fn bulk_archive_uses_the_existing_core_vector_command_from_inbox_to_archive() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let archive = core
            .list_folders(&account)
            .unwrap()
            .into_iter()
            .find(|summary| summary.folder.kind == FolderKind::Archive)
            .unwrap()
            .folder
            .id;
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads;
        let selected = threads[..2]
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>();
        assert!(threads[..2]
            .iter()
            .all(|thread| thread.folder == inbox && !thread.archived()));
        let untouched = threads[2].id.clone();
        assert_eq!(threads[2].folder, inbox);
        assert!(!threads[2].archived());
        let pending_before = core.queue_counts().unwrap().pending;

        let response = call_tool(
            &mut core,
            &Access::default(),
            "bulk_archive",
            &json!({"account_id":account.as_str(),"thread_ids":selected.iter().map(ThreadId::as_str).collect::<Vec<_>>() }),
        )
        .unwrap();

        assert_eq!(response["queued"], true);
        assert_eq!(response.as_object().unwrap().len(), 2);
        assert!(response.get("thread_ids").is_none());
        let operation_ids = response["operation_ids"].as_array().unwrap();
        assert_eq!(operation_ids.len(), 2);
        assert_eq!(
            operation_ids
                .iter()
                .map(|id| id.as_str().unwrap())
                .collect::<HashSet<_>>()
                .len(),
            2
        );
        assert_eq!(core.queue_counts().unwrap().pending, pending_before + 2);
        assert!(selected
            .iter()
            .all(|thread_id| core.get_thread(&account, thread_id).unwrap().archived()));
        assert!(!core.get_thread(&account, &untouched).unwrap().archived());
        assert_eq!(core.get_thread(&account, &untouched).unwrap().folder, inbox);
        let archive_ids = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: archive,
                filter: ThreadFilter::All,
                after: None,
                limit: 10,
            })
            .unwrap()
            .threads
            .into_iter()
            .map(|thread| thread.id)
            .collect::<HashSet<_>>();
        assert!(selected
            .iter()
            .all(|thread_id| archive_ids.contains(thread_id)));
        let inbox_ids = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 10,
            })
            .unwrap()
            .threads
            .into_iter()
            .map(|thread| thread.id)
            .collect::<HashSet<_>>();
        assert!(selected
            .iter()
            .all(|thread_id| !inbox_ids.contains(thread_id)));
        assert!(inbox_ids.contains(&untouched));
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "bulk_archive");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn bulk_archive_fails_closed_for_an_insufficient_or_foreign_ceiling_without_queueing() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let args = json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()]});
        let pending_before = core.queue_counts().unwrap().pending;
        let archived_before = core.get_thread(&account, &thread.id).unwrap().archived();

        for access in [
            Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "bulk_archive", &args).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(
                core.get_thread(&account, &thread.id).unwrap().archived(),
                archived_before
            );
        }
    }

    #[test]
    fn bulk_archive_rejects_malformed_or_unknown_batches_atomically_without_disclosing_ids() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let pending_before = core.queue_counts().unwrap().pending;
        let archived_before = core.get_thread(&account, &thread.id).unwrap().archived();
        assert!(!archived_before);
        let over_limit = Value::Array(
            (0..=MAX_BULK_THREAD_IDS)
                .map(|index| Value::String(format!("batch-{index}")))
                .collect(),
        );

        for args in [
            json!({"account_id":account.as_str()}),
            json!({"account_id":account.as_str(),"thread_ids":[]}),
            json!({"account_id":account.as_str(),"thread_ids":[""]}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),thread.id.as_str()]}),
            json!({"account_id":account.as_str(),"thread_ids":over_limit}),
        ] {
            let error =
                call_tool(&mut core, &Access::default(), "bulk_archive", &args).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(
                core.get_thread(&account, &thread.id).unwrap().archived(),
                archived_before
            );
        }

        let unknown_id = "unknown-bulk-archive-thread";
        let error = call_tool(
            &mut core,
            &Access::default(),
            "bulk_archive",
            &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown_id]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
        assert!(!error.message.contains(unknown_id));
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert_eq!(
            core.get_thread(&account, &thread.id).unwrap().archived(),
            archived_before
        );
        assert!(
            !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(unknown_id),
            "unknown submitted ids must never reach the metadata-only audit"
        );
    }

    #[test]
    fn bulk_star_uses_the_existing_core_vector_command_and_returns_queue_metadata_only() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads;
        let selected = threads[..2]
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>();
        let untouched = threads[2].id.clone();
        let pending_before = core.queue_counts().unwrap().pending;

        let response = call_tool(
            &mut core,
            &Access::default(),
            "bulk_star",
            &json!({"account_id":account.as_str(),"thread_ids":selected.iter().map(ThreadId::as_str).collect::<Vec<_>>() }),
        )
        .unwrap();

        assert_eq!(response["queued"], true);
        assert_eq!(response.as_object().unwrap().len(), 2);
        assert!(response.get("thread_ids").is_none());
        let operation_ids = response["operation_ids"].as_array().unwrap();
        assert_eq!(operation_ids.len(), selected.len());
        assert_eq!(
            operation_ids
                .iter()
                .map(|id| id.as_str().unwrap())
                .collect::<HashSet<_>>()
                .len(),
            selected.len()
        );
        assert_eq!(
            core.queue_counts().unwrap().pending,
            pending_before + u32::try_from(selected.len()).unwrap()
        );
        assert!(selected
            .iter()
            .all(|thread_id| core.get_thread(&account, thread_id).unwrap().starred));
        assert!(!core.get_thread(&account, &untouched).unwrap().starred);
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "bulk_star");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn bulk_star_fails_closed_for_an_insufficient_or_foreign_ceiling_without_queueing() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let args = json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()]});
        let pending_before = core.queue_counts().unwrap().pending;

        for access in [
            Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "bulk_star", &args).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert!(!core.get_thread(&account, &thread.id).unwrap().starred);
        }
    }

    #[test]
    fn bulk_star_rejects_malformed_or_unknown_batches_atomically_without_disclosing_ids() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let pending_before = core.queue_counts().unwrap().pending;
        let over_limit = Value::Array(
            (0..=MAX_BULK_THREAD_IDS)
                .map(|index| Value::String(format!("batch-{index}")))
                .collect(),
        );

        for args in [
            json!({"account_id":account.as_str()}),
            json!({"account_id":account.as_str(),"thread_ids":[]}),
            json!({"account_id":account.as_str(),"thread_ids":[""]}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),thread.id.as_str()]}),
            json!({"account_id":account.as_str(),"thread_ids":over_limit}),
        ] {
            let error = call_tool(&mut core, &Access::default(), "bulk_star", &args).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert!(!core.get_thread(&account, &thread.id).unwrap().starred);
        }

        let unknown_id = "unknown-bulk-star-thread";
        let error = call_tool(
            &mut core,
            &Access::default(),
            "bulk_star",
            &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown_id]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
        assert!(!error.message.contains(unknown_id));
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert!(!core.get_thread(&account, &thread.id).unwrap().starred);
        assert!(
            !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(unknown_id),
            "unknown submitted ids must never reach the metadata-only audit"
        );
    }

    #[test]
    fn bulk_unstar_uses_the_existing_core_vector_command_and_returns_queue_metadata_only() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads;
        let selected = threads[..2]
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>();
        let untouched = threads[2].id.clone();
        core.dispatch(Command::Star {
            account_id: account.clone(),
            thread_ids: selected.clone(),
        })
        .unwrap();
        let pending_before = core.queue_counts().unwrap().pending;

        let response = call_tool(
            &mut core,
            &Access::default(),
            "bulk_unstar",
            &json!({"account_id":account.as_str(),"thread_ids":selected.iter().map(ThreadId::as_str).collect::<Vec<_>>() }),
        )
        .unwrap();

        assert_eq!(response["queued"], true);
        assert_eq!(response.as_object().unwrap().len(), 2);
        assert!(response.get("thread_ids").is_none());
        let operation_ids = response["operation_ids"].as_array().unwrap();
        assert_eq!(operation_ids.len(), selected.len());
        assert_eq!(
            operation_ids
                .iter()
                .map(|id| id.as_str().unwrap())
                .collect::<HashSet<_>>()
                .len(),
            selected.len()
        );
        assert_eq!(
            core.queue_counts().unwrap().pending,
            pending_before + u32::try_from(selected.len()).unwrap()
        );
        assert!(selected
            .iter()
            .all(|thread_id| !core.get_thread(&account, thread_id).unwrap().starred));
        assert!(!core.get_thread(&account, &untouched).unwrap().starred);
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "bulk_unstar");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn bulk_unstar_fails_closed_for_an_insufficient_or_foreign_ceiling_without_queueing() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        core.dispatch(Command::Star {
            account_id: account.clone(),
            thread_ids: vec![thread.id.clone()],
        })
        .unwrap();
        let args = json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()]});
        let pending_before = core.queue_counts().unwrap().pending;

        for access in [
            Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "bulk_unstar", &args).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert!(core.get_thread(&account, &thread.id).unwrap().starred);
        }
    }

    #[test]
    fn bulk_unstar_rejects_malformed_or_unknown_batches_atomically_without_disclosing_ids() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        core.dispatch(Command::Star {
            account_id: account.clone(),
            thread_ids: vec![thread.id.clone()],
        })
        .unwrap();
        let pending_before = core.queue_counts().unwrap().pending;
        let over_limit = Value::Array(
            (0..=MAX_BULK_THREAD_IDS)
                .map(|index| Value::String(format!("batch-{index}")))
                .collect(),
        );

        for args in [
            json!({"account_id":account.as_str()}),
            json!({"account_id":account.as_str(),"thread_ids":[]}),
            json!({"account_id":account.as_str(),"thread_ids":[""]}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),thread.id.as_str()]}),
            json!({"account_id":account.as_str(),"thread_ids":over_limit}),
        ] {
            let error = call_tool(&mut core, &Access::default(), "bulk_unstar", &args).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert!(core.get_thread(&account, &thread.id).unwrap().starred);
        }

        let unknown_id = "unknown-bulk-unstar-thread";
        let error = call_tool(
            &mut core,
            &Access::default(),
            "bulk_unstar",
            &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown_id]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
        assert!(!error.message.contains(unknown_id));
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert!(core.get_thread(&account, &thread.id).unwrap().starred);
        assert!(
            !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(unknown_id),
            "unknown submitted ids must never reach the metadata-only audit"
        );
    }

    #[test]
    fn bulk_move_uses_the_existing_core_vector_command_for_a_custom_folder() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let destination = core.create_folder(&account, "Projects").unwrap();
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads;
        let selected = threads[..2]
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>();
        let untouched = threads[2].id.clone();
        let pending_before = core.queue_counts().unwrap().pending;

        let response = call_tool(
            &mut core,
            &Access::default(),
            "bulk_move",
            &json!({"account_id":account.as_str(),"folder_id":destination.as_str(),"thread_ids":selected.iter().map(ThreadId::as_str).collect::<Vec<_>>() }),
        )
        .unwrap();

        assert_eq!(response["queued"], true);
        assert_eq!(response.as_object().unwrap().len(), 2);
        assert!(response.get("thread_ids").is_none());
        let operation_ids = response["operation_ids"].as_array().unwrap();
        assert_eq!(operation_ids.len(), selected.len());
        assert_eq!(
            operation_ids
                .iter()
                .map(|id| id.as_str().unwrap())
                .collect::<HashSet<_>>()
                .len(),
            selected.len()
        );
        assert_eq!(
            core.queue_counts().unwrap().pending,
            pending_before + u32::try_from(selected.len()).unwrap()
        );
        assert!(selected.iter().all(|thread_id| {
            core.get_thread(&account, thread_id).unwrap().folder == destination
        }));
        assert_eq!(core.get_thread(&account, &untouched).unwrap().folder, inbox);
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "bulk_move");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn bulk_move_fails_closed_for_an_insufficient_or_foreign_ceiling_without_queueing() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let destination = core.create_folder(&account, "Projects").unwrap();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let args = json!({"account_id":account.as_str(),"folder_id":destination.as_str(),"thread_ids":[thread.id.as_str()]});
        let pending_before = core.queue_counts().unwrap().pending;

        for access in [
            Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "bulk_move", &args).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(core.get_thread(&account, &thread.id).unwrap().folder, inbox);
        }
    }

    #[test]
    fn bulk_move_rejects_malformed_or_unknown_batches_atomically_without_disclosing_ids() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let destination = core.create_folder(&account, "Projects").unwrap();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let pending_before = core.queue_counts().unwrap().pending;
        let over_limit = Value::Array(
            (0..=MAX_BULK_THREAD_IDS)
                .map(|index| Value::String(format!("batch-{index}")))
                .collect(),
        );

        for args in [
            json!({"account_id":account.as_str(),"folder_id":destination.as_str()}),
            json!({"account_id":account.as_str(),"folder_id":destination.as_str(),"thread_ids":[]}),
            json!({"account_id":account.as_str(),"folder_id":destination.as_str(),"thread_ids":[""]}),
            json!({"account_id":account.as_str(),"folder_id":destination.as_str(),"thread_ids":[thread.id.as_str(),thread.id.as_str()]}),
            json!({"account_id":account.as_str(),"folder_id":destination.as_str(),"thread_ids":over_limit}),
        ] {
            let error = call_tool(&mut core, &Access::default(), "bulk_move", &args).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(core.get_thread(&account, &thread.id).unwrap().folder, inbox);
        }

        let unknown_id = "unknown-bulk-move-thread";
        let error = call_tool(
            &mut core,
            &Access::default(),
            "bulk_move",
            &json!({"account_id":account.as_str(),"folder_id":destination.as_str(),"thread_ids":[thread.id.as_str(),unknown_id]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
        assert!(!error.message.contains(unknown_id));
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert_eq!(core.get_thread(&account, &thread.id).unwrap().folder, inbox);
        assert!(
            !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(unknown_id),
            "unknown submitted ids must never reach the metadata-only audit"
        );
    }

    #[test]
    fn bulk_move_rejects_every_non_custom_destination_before_dispatch() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let other_account = core
            .add_account(
                &MailboxForm {
                    email: "other-cursor@example.test".into(),
                    imap_host: "imap.example.test".into(),
                    imap_port: 993,
                    imap_security: MailSecurity::Ssl,
                    smtp_host: "smtp.example.test".into(),
                    smtp_port: 465,
                    smtp_security: MailSecurity::Ssl,
                },
                "x",
                &LocalProbe,
            )
            .unwrap();
        let foreign_destination = core.create_folder(&other_account, "Elsewhere").unwrap();
        let pending_before = core.queue_counts().unwrap().pending;

        for folder_id in [
            inbox.as_str(),
            "snoozed",
            "not-a-listed-folder",
            foreign_destination.as_str(),
        ] {
            let error = call_tool(
                &mut core,
                &Access::default(),
                "bulk_move",
                &json!({"account_id":account.as_str(),"folder_id":folder_id,"thread_ids":[thread.id.as_str()]}),
            )
            .unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert!(!error.message.contains(folder_id));
            assert!(
                !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(folder_id),
                "rejected destination ids must never reach the metadata-only audit"
            );
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(core.get_thread(&account, &thread.id).unwrap().folder, inbox);
        }
    }

    #[test]
    fn bulk_delete_binds_allow_once_to_the_exact_canonical_batch_and_trashes_atomically() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads;
        let selected = threads[..2]
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>();
        let untouched = threads[2].id.clone();
        let args = json!({
            "account_id": account.as_str(),
            "thread_ids": selected.iter().map(ThreadId::as_str).collect::<Vec<_>>(),
        });
        let reversed = json!({
            "account_id": account.as_str(),
            "thread_ids": selected.iter().rev().map(ThreadId::as_str).collect::<Vec<_>>(),
        });
        let access = Access {
            ceiling: PermissionLevel::Full,
            ..Access::default()
        };
        let pending_before = core.queue_counts().unwrap().pending;

        let first = call_tool(&mut core, &access, "bulk_delete", &args).unwrap_err();
        assert_eq!(first.code, "PERMISSION_DENIED");
        let request_id = first.pending_confirmation().unwrap();
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert!(selected
            .iter()
            .all(|id| !core.get_thread(&account, id).unwrap().deleted()));

        let request = core
            .list_pending_mcp_confirmations(1)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(request.id, request_id);
        assert_eq!(request.capability, "bulk_delete");
        assert_eq!(request.target_id, None);
        assert_eq!(request.target_count, selected.len() as u32);

        let same = call_tool(&mut core, &access, "bulk_delete", &reversed).unwrap_err();
        assert_eq!(same.pending_confirmation(), Some(request_id));
        let changed = call_tool(
            &mut core,
            &access,
            "bulk_delete",
            &json!({
                "account_id": account.as_str(),
                "thread_ids": [selected[0].as_str(), untouched.as_str()],
            }),
        )
        .unwrap_err();
        assert_ne!(changed.pending_confirmation(), Some(request_id));
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);

        assert!(core
            .resolve_mcp_confirmation(request_id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        let response = call_tool(&mut core, &access, "bulk_delete", &reversed).unwrap();
        assert_eq!(response["queued"], true);
        assert_eq!(response.as_object().unwrap().len(), 2);
        assert!(response.get("thread_ids").is_none());
        let operation_ids = response["operation_ids"].as_array().unwrap();
        assert_eq!(operation_ids.len(), selected.len());
        assert_eq!(
            operation_ids
                .iter()
                .map(|id| id.as_str().unwrap())
                .collect::<HashSet<_>>()
                .len(),
            selected.len()
        );
        assert_eq!(
            core.queue_counts().unwrap().pending,
            pending_before + selected.len() as u32
        );
        assert!(selected
            .iter()
            .all(|id| core.get_thread(&account, id).unwrap().deleted()));
        assert!(!core.get_thread(&account, &untouched).unwrap().deleted());
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "bulk_delete");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn bulk_delete_rejects_unsafe_input_and_insufficient_or_foreign_access_without_mutation() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let valid = json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()]});
        let pending_before = core.queue_counts().unwrap().pending;

        for access in [
            Access::default(),
            Access {
                ceiling: PermissionLevel::Send,
                ..Access::default()
            },
            Access {
                ceiling: PermissionLevel::Full,
                accounts: HashSet::from(["another-account".to_string()]),
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "bulk_delete", &valid).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert!(error.pending_confirmation().is_none());
            assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        }

        let over_limit = Value::Array(
            (0..=MAX_BULK_THREAD_IDS)
                .map(|index| Value::String(format!("batch-{index}")))
                .collect(),
        );
        let full = Access {
            ceiling: PermissionLevel::Full,
            ..Access::default()
        };
        for args in [
            json!({"account_id":account.as_str()}),
            json!({"account_id":account.as_str(),"thread_ids":[]}),
            json!({"account_id":account.as_str(),"thread_ids":[""]}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),thread.id.as_str()]}),
            json!({"account_id":account.as_str(),"thread_ids":over_limit}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()],"confirm":true}),
        ] {
            let error = call_tool(&mut core, &full, "bulk_delete", &args).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert!(error.pending_confirmation().is_none());
            assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        }

        let unknown = "unknown-bulk-delete-thread";
        let pending = call_tool(
            &mut core,
            &full,
            "bulk_delete",
            &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown]}),
        )
        .unwrap_err();
        let request_id = pending.pending_confirmation().unwrap();
        assert!(core
            .resolve_mcp_confirmation(request_id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        let error = call_tool(
            &mut core,
            &full,
            "bulk_delete",
            &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
        assert!(!error.message.contains(unknown));
        assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        let retry = call_tool(
            &mut core,
            &full,
            "bulk_delete",
            &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown]}),
        )
        .unwrap_err();
        assert_eq!(
            retry.code, "MESSAGE_NOT_FOUND",
            "a failed atomic dispatch must roll back Allow-once rather than consume it"
        );
        assert!(retry.pending_confirmation().is_none());
        assert!(
            !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(unknown),
            "unknown submitted ids must never reach the metadata-only audit"
        );
    }

    #[test]
    fn bulk_permanent_delete_binds_allow_once_to_the_exact_batch_without_undo() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads;
        let selected = threads[..2]
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>();
        let untouched = threads[2].id.clone();
        let args = json!({
            "account_id": account.as_str(),
            "thread_ids": selected.iter().map(ThreadId::as_str).collect::<Vec<_>>(),
        });
        let reversed = json!({
            "account_id": account.as_str(),
            "thread_ids": selected.iter().rev().map(ThreadId::as_str).collect::<Vec<_>>(),
        });
        let access = Access {
            ceiling: PermissionLevel::Full,
            ..Access::default()
        };
        let pending_before = core.queue_counts().unwrap().pending;

        let first = call_tool(&mut core, &access, "bulk_permanent_delete", &args).unwrap_err();
        let request_id = first.pending_confirmation().unwrap();
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        let request = core
            .list_pending_mcp_confirmations(1)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(request.id, request_id);
        assert_eq!(request.capability, "bulk_permanent_delete");
        assert_eq!(request.target_id, None);
        assert_eq!(request.target_count, selected.len() as u32);

        let same = call_tool(&mut core, &access, "bulk_permanent_delete", &reversed).unwrap_err();
        assert_eq!(same.pending_confirmation(), Some(request_id));
        let changed = call_tool(
            &mut core,
            &access,
            "bulk_permanent_delete",
            &json!({
                "account_id": account.as_str(),
                "thread_ids": [selected[0].as_str(), untouched.as_str()],
            }),
        )
        .unwrap_err();
        assert_ne!(changed.pending_confirmation(), Some(request_id));

        assert!(core
            .resolve_mcp_confirmation(request_id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        let response = call_tool(&mut core, &access, "bulk_permanent_delete", &reversed).unwrap();
        assert_eq!(response["queued"], true);
        let operation_ids = response["operation_ids"].as_array().unwrap();
        assert_eq!(operation_ids.len(), selected.len());
        assert_eq!(
            core.queue_counts().unwrap().pending,
            pending_before + selected.len() as u32
        );
        assert!(selected
            .iter()
            .all(|id| core.get_thread(&account, id).unwrap().deleted()));
        assert!(!core.get_thread(&account, &untouched).unwrap().deleted());
        for operation_id in operation_ids {
            let ticket = UndoTicket {
                operation_id: OperationId(operation_id.as_str().unwrap().into()),
            };
            assert_eq!(
                core.undo(&ticket).unwrap_err().code,
                ErrorCode::OperationNotSupported
            );
        }
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "bulk_permanent_delete");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn bulk_permanent_delete_rejects_unsafe_or_unknown_batches_without_a_partial_mutation() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let valid = json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()]});
        let pending_before = core.queue_counts().unwrap().pending;
        for access in [
            Access::default(),
            Access {
                ceiling: PermissionLevel::Send,
                ..Access::default()
            },
            Access {
                ceiling: PermissionLevel::Full,
                accounts: HashSet::from(["another-account".to_string()]),
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "bulk_permanent_delete", &valid).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert!(error.pending_confirmation().is_none());
            assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        }

        let too_many = Value::Array(
            (0..=MAX_BULK_THREAD_IDS)
                .map(|index| Value::String(format!("permanent-{index}")))
                .collect(),
        );
        let full = Access {
            ceiling: PermissionLevel::Full,
            ..Access::default()
        };
        for args in [
            json!({"account_id":account.as_str()}),
            json!({"account_id":account.as_str(),"thread_ids":[]}),
            json!({"account_id":account.as_str(),"thread_ids":[""]}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),thread.id.as_str()]}),
            json!({"account_id":account.as_str(),"thread_ids":too_many}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()],"confirm":true}),
        ] {
            let error = call_tool(&mut core, &full, "bulk_permanent_delete", &args).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert!(error.pending_confirmation().is_none());
            assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
        }

        let unknown = "unknown-bulk-permanent-thread";
        let pending = call_tool(
            &mut core,
            &full,
            "bulk_permanent_delete",
            &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown]}),
        )
        .unwrap_err();
        let request_id = pending.pending_confirmation().unwrap();
        assert!(core
            .resolve_mcp_confirmation(request_id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        for _ in 0..2 {
            let error = call_tool(
                &mut core,
                &full,
                "bulk_permanent_delete",
                &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown]}),
            )
            .unwrap_err();
            assert_eq!(error.code, "MESSAGE_NOT_FOUND");
            assert!(error.pending_confirmation().is_none());
            assert!(!error.message.contains(unknown));
        }
        assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert!(
            !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(unknown),
            "unknown submitted ids must never reach the metadata-only audit"
        );
    }

    #[test]
    fn bulk_high_risk_always_allow_is_per_tool_and_rechecks_the_live_client_policy() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads;
        let first = threads[0].id.clone();
        let second = threads[1].id.clone();
        let third = threads[2].id.clone();
        let access = Access {
            ceiling: PermissionLevel::Full,
            ..Access::default()
        };

        let request = call_tool(
            &mut core,
            &access,
            "bulk_delete",
            &json!({"account_id":account.as_str(),"thread_ids":[first.as_str()]}),
        )
        .unwrap_err()
        .pending_confirmation()
        .unwrap();
        assert!(core
            .resolve_mcp_confirmation(request, McpConfirmationChoice::AlwaysAllow)
            .unwrap());

        assert!(call_tool(
            &mut core,
            &access,
            "bulk_delete",
            &json!({"account_id":account.as_str(),"thread_ids":[second.as_str()]}),
        )
        .is_ok());
        assert!(core.get_thread(&account, &second).unwrap().deleted());

        let permanent_request = call_tool(
            &mut core,
            &access,
            "bulk_permanent_delete",
            &json!({"account_id":account.as_str(),"thread_ids":[first.as_str()]}),
        )
        .unwrap_err()
        .pending_confirmation()
        .unwrap();
        assert!(core
            .resolve_mcp_confirmation(permanent_request, McpConfirmationChoice::AlwaysAllow)
            .unwrap());
        assert!(call_tool(
            &mut core,
            &access,
            "bulk_permanent_delete",
            &json!({"account_id":account.as_str(),"thread_ids":[first.as_str()]}),
        )
        .is_ok());
        assert!(core.get_thread(&account, &first).unwrap().deleted());

        let other_tool = call_tool(
            &mut core,
            &access,
            "delete_message",
            &json!({"account_id":account.as_str(),"thread_id":first.as_str()}),
        )
        .unwrap_err();
        assert!(other_tool.pending_confirmation().is_some());

        let pending_before_revoke = core.queue_counts().unwrap().pending;
        core.revoke_mcp_client("stdio").unwrap();
        let revoked = call_tool(
            &mut core,
            &access,
            "bulk_delete",
            &json!({"account_id":account.as_str(),"thread_ids":[third.as_str()]}),
        )
        .unwrap_err();
        assert_eq!(revoked.code, "PERMISSION_DENIED");
        assert!(revoked.pending_confirmation().is_none());
        assert!(!core.get_thread(&account, &third).unwrap().deleted());
        assert_eq!(core.queue_counts().unwrap().pending, pending_before_revoke);
    }

    #[test]
    fn bulk_snooze_uses_the_existing_core_vector_command_with_one_deadline() {
        let (mut core, account, inbox) = seeded_cursor_core();
        core.set_now(1_000);
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 3,
            })
            .unwrap()
            .threads;
        let selected = threads[..2]
            .iter()
            .map(|thread| thread.id.clone())
            .collect::<Vec<_>>();
        let untouched = threads[2].id.clone();
        let until = 2_000;
        let pending_before = core.queue_counts().unwrap().pending;

        let response = call_tool(
            &mut core,
            &Access::default(),
            "bulk_snooze",
            &json!({"account_id":account.as_str(),"thread_ids":selected.iter().map(ThreadId::as_str).collect::<Vec<_>>(),"until":until}),
        )
        .unwrap();

        assert_eq!(response["queued"], true);
        assert_eq!(response.as_object().unwrap().len(), 2);
        assert!(response.get("thread_ids").is_none());
        let operation_ids = response["operation_ids"].as_array().unwrap();
        assert_eq!(operation_ids.len(), selected.len());
        assert_eq!(
            operation_ids
                .iter()
                .map(|id| id.as_str().unwrap())
                .collect::<HashSet<_>>()
                .len(),
            selected.len()
        );
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert!(selected.iter().all(|thread_id| {
            core.get_thread(&account, thread_id)
                .unwrap()
                .snoozed_until()
                == Some(until)
        }));
        let untouched_thread = core.get_thread(&account, &untouched).unwrap();
        assert_eq!(untouched_thread.folder, inbox);
        assert_eq!(untouched_thread.snoozed_until(), None);
        let snoozed_ids = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: FolderId("snoozed".into()),
                filter: ThreadFilter::All,
                after: None,
                limit: 10,
            })
            .unwrap()
            .threads
            .into_iter()
            .map(|thread| thread.id)
            .collect::<HashSet<_>>();
        assert!(selected
            .iter()
            .all(|thread_id| snoozed_ids.contains(thread_id)));
        assert!(!snoozed_ids.contains(&untouched));
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "bulk_snooze");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn bulk_snooze_fails_closed_for_an_insufficient_or_foreign_ceiling_without_local_receipts() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let args =
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()],"until":2_000});
        let pending_before = core.queue_counts().unwrap().pending;

        for access in [
            Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "bulk_snooze", &args).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(core.get_thread(&account, &thread.id).unwrap().folder, inbox);
            assert_eq!(
                core.get_thread(&account, &thread.id)
                    .unwrap()
                    .snoozed_until(),
                None
            );
        }
    }

    #[test]
    fn bulk_snooze_rejects_malformed_or_unknown_input_atomically_without_disclosing_ids() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let pending_before = core.queue_counts().unwrap().pending;
        let over_limit = Value::Array(
            (0..=MAX_BULK_THREAD_IDS)
                .map(|index| Value::String(format!("batch-{index}")))
                .collect(),
        );

        for args in [
            json!({"account_id":account.as_str(),"until":2_000}),
            json!({"account_id":account.as_str(),"thread_ids":[],"until":2_000}),
            json!({"account_id":account.as_str(),"thread_ids":[""],"until":2_000}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),thread.id.as_str()],"until":2_000}),
            json!({"account_id":account.as_str(),"thread_ids":over_limit,"until":2_000}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()]}),
            json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str()],"until":"tomorrow"}),
        ] {
            let error = call_tool(&mut core, &Access::default(), "bulk_snooze", &args).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
            assert_eq!(core.get_thread(&account, &thread.id).unwrap().folder, inbox);
            assert_eq!(
                core.get_thread(&account, &thread.id)
                    .unwrap()
                    .snoozed_until(),
                None
            );
        }

        let unknown_id = "unknown-bulk-snooze-thread";
        let error = call_tool(
            &mut core,
            &Access::default(),
            "bulk_snooze",
            &json!({"account_id":account.as_str(),"thread_ids":[thread.id.as_str(),unknown_id],"until":2_000}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
        assert!(!error.message.contains(unknown_id));
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert_eq!(core.get_thread(&account, &thread.id).unwrap().folder, inbox);
        assert_eq!(
            core.get_thread(&account, &thread.id)
                .unwrap()
                .snoozed_until(),
            None
        );
        assert!(
            !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(unknown_id),
            "unknown submitted ids must never reach the metadata-only audit"
        );
    }

    fn first_inbox_thread(core: &mut Core, account: &AccountId, inbox: &FolderId) -> ThreadId {
        core.list_threads(ListThreadsQuery {
            account_id: account.clone(),
            folder_id: inbox.clone(),
            filter: ThreadFilter::All,
            after: None,
            limit: 1,
        })
        .unwrap()
        .threads
        .into_iter()
        .next()
        .unwrap()
        .id
    }

    #[test]
    fn unsnooze_message_brings_a_snoozed_thread_back_without_queueing_imap() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = first_inbox_thread(&mut core, &account, &inbox);
        let until = 4_102_444_800; // 2100-01-01, safely past any test clock
        call_tool(
            &mut core,
            &Access::default(),
            "snooze_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.as_str(),"until":until}),
        )
        .unwrap();
        assert_eq!(
            core.next_snooze_deadline(Some(&account)).unwrap(),
            Some(until)
        );

        let response = call_tool(
            &mut core,
            &Access::default(),
            "unsnooze_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.as_str()}),
        )
        .unwrap();
        assert_eq!(response["unsnoozed"], json!(true));
        // Snooze is a local overlay (D26): nothing is owed to the server.
        assert_eq!(response["queued"], json!(false));
        assert_eq!(core.next_snooze_deadline(Some(&account)).unwrap(), None);
        assert_eq!(
            core.get_thread(&account, &thread).unwrap().folder.as_str(),
            inbox.as_str()
        );

        // Asking again is not an error; it reports that there was nothing left.
        let repeat = call_tool(
            &mut core,
            &Access::default(),
            "unsnooze_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.as_str()}),
        )
        .unwrap();
        assert_eq!(repeat["unsnoozed"], json!(false));
    }

    #[test]
    fn unsnooze_message_refuses_a_read_only_client() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = first_inbox_thread(&mut core, &account, &inbox);
        let until = 4_102_444_800; // 2100-01-01, safely past any test clock
        call_tool(
            &mut core,
            &Access::default(),
            "snooze_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.as_str(),"until":until}),
        )
        .unwrap();
        let error = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            "unsnooze_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
        assert_eq!(
            core.next_snooze_deadline(Some(&account)).unwrap(),
            Some(until),
            "a refused call must not have unsnoozed anything"
        );
    }

    #[test]
    fn unsnooze_message_rejects_an_account_outside_the_ceiling() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = first_inbox_thread(&mut core, &account, &inbox);
        let error = call_tool(
            &mut core,
            &Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
            "unsnooze_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
    }

    #[test]
    fn unsnooze_message_is_a_modify_action_like_snooze_itself() {
        let snooze = tool_policy("snooze_message").unwrap();
        let unsnooze = tool_policy("unsnooze_message").unwrap();
        assert_eq!(unsnooze.capability, snooze.capability);
        assert_eq!(unsnooze.required, snooze.required);
    }

    /// A folder made by `create_folder` exists locally with no `remote_id`
    /// until a `LIST` walk adopts it. Renaming needs that identity, so these
    /// tests hand it over the same way the real sync does.
    fn adopt_on_server(core: &mut Core, account: &AccountId, remote_id: &str) {
        core.sync_folders(
            account,
            &[feathermail_core::remote::DiscoveredFolder {
                remote_id: remote_id.into(),
                kind: FolderKind::Custom,
                label: remote_id.into(),
                parent_remote_id: None,
                delimiter: Some('/'),
            }],
        )
        .unwrap();
    }

    fn folder_label(core: &Core, account: &AccountId, id: &FolderId) -> String {
        core.list_folders(account)
            .unwrap()
            .into_iter()
            .find(|summary| &summary.folder.id == id)
            .unwrap()
            .folder
            .label
    }

    #[test]
    fn rename_folder_renames_through_core_and_reports_whether_it_queued() {
        let (mut core, account, _) = seeded_cursor_core();
        let folder = core.create_folder(&account, "Projects").unwrap();
        adopt_on_server(&mut core, &account, "Projects");

        let response = call_tool(
            &mut core,
            &Access::default(),
            "rename_folder",
            &json!({"account_id":account.as_str(),"folder_id":folder.as_str(),"name":"Plans"}),
        )
        .unwrap();
        assert_eq!(response["queued"], true);
        assert_eq!(response["folder_id"], folder.as_str());
        assert_eq!(folder_label(&core, &account, &folder), "Plans");

        // Asking for the name it already has is a satisfied request, not an
        // error -- but it must not put a `RENAME x x` on the wire.
        let again = call_tool(
            &mut core,
            &Access::default(),
            "rename_folder",
            &json!({"account_id":account.as_str(),"folder_id":folder.as_str(),"name":"Plans"}),
        )
        .unwrap();
        assert_eq!(again["queued"], false);
        assert_eq!(folder_label(&core, &account, &folder), "Plans");
    }

    #[test]
    fn rename_folder_propagates_every_core_refusal_verbatim() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let folder = core.create_folder(&account, "Projects").unwrap();
        let local_only = call_tool(
            &mut core,
            &Access::default(),
            "rename_folder",
            &json!({"account_id":account.as_str(),"folder_id":folder.as_str(),"name":"Plans"}),
        )
        .unwrap_err();
        assert_eq!(local_only.code, "INVALID_ARGUMENT");
        assert_eq!(
            local_only.message,
            "Wait until this folder reaches the server."
        );

        adopt_on_server(&mut core, &account, "Projects");
        let system = call_tool(
            &mut core,
            &Access::default(),
            "rename_folder",
            &json!({"account_id":account.as_str(),"folder_id":inbox.as_str(),"name":"Plans"}),
        )
        .unwrap_err();
        assert_eq!(system.message, "System folders can\u{2019}t be renamed.");

        let reserved = call_tool(
            &mut core,
            &Access::default(),
            "rename_folder",
            &json!({"account_id":account.as_str(),"folder_id":folder.as_str(),"name":"Trash"}),
        )
        .unwrap_err();
        assert_eq!(reserved.message, "That name is a system folder.");

        let missing = call_tool(
            &mut core,
            &Access::default(),
            "rename_folder",
            &json!({"account_id":account.as_str(),"folder_id":"you:nope","name":"Plans"}),
        )
        .unwrap_err();
        assert_eq!(missing.code, "INVALID_ARGUMENT");
        assert_eq!(folder_label(&core, &account, &folder), "Projects");
    }

    #[test]
    fn rename_folder_refuses_an_account_outside_the_ceiling() {
        let (mut core, account, _) = seeded_cursor_core();
        let folder = core.create_folder(&account, "Projects").unwrap();
        let error = call_tool(
            &mut core,
            &Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
            "rename_folder",
            &json!({"account_id":account.as_str(),"folder_id":folder.as_str(),"name":"Plans"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
        assert_eq!(folder_label(&core, &account, &folder), "Projects");
    }

    #[test]
    fn rename_folder_is_a_modify_action_exactly_like_create_folder() {
        let create = tool_policy("create_folder").unwrap();
        let rename = tool_policy("rename_folder").unwrap();
        assert_eq!(rename.capability, create.capability);
        assert_eq!(rename.required, create.required);
    }

    #[test]
    fn rename_folder_needs_draft_and_is_refused_at_read() {
        let (mut core, account, _) = seeded_cursor_core();
        let folder = core.create_folder(&account, "Projects").unwrap();
        adopt_on_server(&mut core, &account, "Projects");
        let error = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            "rename_folder",
            &json!({"account_id":account.as_str(),"folder_id":folder.as_str(),"name":"Plans"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
        assert_eq!(folder_label(&core, &account, &folder), "Projects");
    }

    fn folder_is_listed(core: &Core, account: &AccountId, id: &FolderId) -> bool {
        core.list_folders(account)
            .unwrap()
            .iter()
            .any(|summary| &summary.folder.id == id)
    }

    #[test]
    fn delete_folder_needs_a_folder_bound_approval_and_then_deletes_through_core() {
        let (mut core, account, _) = seeded_cursor_core();
        let folder = core.create_folder(&account, "Projects").unwrap();
        let other = core.create_folder(&account, "Plans").unwrap();
        adopt_on_server(&mut core, &account, "Projects");
        let access = Access {
            ceiling: PermissionLevel::Full,
            ..Access::default()
        };
        let args = json!({"account_id":account.as_str(),"folder_id":folder.as_str()});

        let first = call_tool(&mut core, &access, "delete_folder", &args).unwrap_err();
        assert_eq!(first.code, "PERMISSION_DENIED");
        let request_id = first.pending_confirmation().unwrap();
        assert!(
            folder_is_listed(&core, &account, &folder),
            "asking is not doing"
        );

        let request = core
            .list_pending_mcp_confirmations(1)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(request.id, request_id);
        assert_eq!(request.capability, "delete_folder");
        assert_eq!(request.target_count, 1);

        // A different folder is a different decision, even for the same client.
        let elsewhere = call_tool(
            &mut core,
            &access,
            "delete_folder",
            &json!({"account_id":account.as_str(),"folder_id":other.as_str()}),
        )
        .unwrap_err();
        assert_ne!(elsewhere.pending_confirmation(), Some(request_id));

        assert!(core
            .resolve_mcp_confirmation(request_id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        let response = call_tool(&mut core, &access, "delete_folder", &args).unwrap();
        assert_eq!(response["deleted"], true);
        assert_eq!(response["queued"], true);
        assert_eq!(response["folder_id"], folder.as_str());
        assert!(!folder_is_listed(&core, &account, &folder));
        assert!(
            folder_is_listed(&core, &account, &other),
            "one approval deletes one folder"
        );
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "delete_folder");
        assert_eq!(audit[0].outcome, "ok");
    }

    /// The refusals a user would get reach an agent unchanged, and none of
    /// them happens before the approval: an agent must not be able to probe
    /// which folders hold mail by watching which calls fail differently.
    #[test]
    fn delete_folder_propagates_core_refusals_after_the_approval() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let access = Access {
            ceiling: PermissionLevel::Full,
            ..Access::default()
        };

        let args = json!({"account_id":account.as_str(),"folder_id":inbox.as_str()});
        let asked = call_tool(&mut core, &access, "delete_folder", &args).unwrap_err();
        let request_id = asked.pending_confirmation().unwrap();
        assert!(core
            .resolve_mcp_confirmation(request_id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        let system = call_tool(&mut core, &access, "delete_folder", &args).unwrap_err();
        assert_eq!(system.code, "INVALID_ARGUMENT");
        assert_eq!(system.message, "System folders can\u{2019}t be deleted.");
        assert!(folder_is_listed(&core, &account, &inbox));

        // Inbox holds the fixture's mail, so the emptiness rule is what a
        // *custom* folder has to prove separately.
        let full = core.create_folder(&account, "Projects").unwrap();
        let destination = json!({
            "account_id": account.as_str(),
            "thread_id": core
                .list_threads(ListThreadsQuery {
                    account_id: account.clone(),
                    folder_id: inbox.clone(),
                    filter: ThreadFilter::All,
                    after: None,
                    limit: 1,
                })
                .unwrap()
                .threads[0]
                .id
                .as_str(),
            "folder_id": full.as_str(),
        });
        call_tool(&mut core, &Access::default(), "move_message", &destination).unwrap();

        let full_args = json!({"account_id":account.as_str(),"folder_id":full.as_str()});
        let asked = call_tool(&mut core, &access, "delete_folder", &full_args).unwrap_err();
        let request_id = asked.pending_confirmation().unwrap();
        assert!(core
            .resolve_mcp_confirmation(request_id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        let not_empty = call_tool(&mut core, &access, "delete_folder", &full_args).unwrap_err();
        assert_eq!(
            not_empty.message,
            "Move this folder\u{2019}s mail out first."
        );
        assert!(folder_is_listed(&core, &account, &full));
    }

    #[test]
    fn delete_folder_is_as_high_risk_as_a_permanent_delete() {
        let permanent = tool_policy("bulk_permanent_delete").unwrap();
        let folder = tool_policy("delete_folder").unwrap();
        assert_eq!(folder.capability, permanent.capability);
        assert_eq!(folder.required, permanent.required);
    }

    #[test]
    fn delete_folder_refuses_a_low_ceiling_or_a_foreign_account_without_asking() {
        let (mut core, account, _) = seeded_cursor_core();
        let folder = core.create_folder(&account, "Projects").unwrap();
        adopt_on_server(&mut core, &account, "Projects");
        let args = json!({"account_id":account.as_str(),"folder_id":folder.as_str()});

        // Draft ceiling: denied outright, with no confirmation to approve.
        let low = call_tool(&mut core, &Access::default(), "delete_folder", &args).unwrap_err();
        assert_eq!(low.code, "PERMISSION_DENIED");
        assert_eq!(low.pending_confirmation(), None);

        let foreign = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Full,
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
            "delete_folder",
            &args,
        )
        .unwrap_err();
        assert_eq!(foreign.code, "PERMISSION_DENIED");
        assert_eq!(foreign.pending_confirmation(), None);

        // And MCP may not confirm on the user's behalf.
        let self_confirmed = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Full,
                ..Access::default()
            },
            "delete_folder",
            &json!({"account_id":account.as_str(),"folder_id":folder.as_str(),"confirm":true}),
        )
        .unwrap_err();
        assert_eq!(self_confirmed.code, "INVALID_ARGUMENT");
        assert!(folder_is_listed(&core, &account, &folder));
    }

    #[test]
    fn move_message_uses_the_existing_core_dispatch_for_a_custom_folder() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let destination = core.create_folder(&account, "Projects").unwrap();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();

        let invalid = call_tool(
            &mut core,
            &Access::default(),
            "move_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.id.as_str(),"folder_id":"not-a-listed-folder"}),
        )
        .unwrap_err();
        assert_eq!(invalid.code, "INVALID_ARGUMENT");
        assert_eq!(core.get_thread(&account, &thread.id).unwrap().folder, inbox);

        let virtual_destination = call_tool(
            &mut core,
            &Access::default(),
            "move_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.id.as_str(),"folder_id":"snoozed"}),
        )
        .unwrap_err();
        assert_eq!(virtual_destination.code, "INVALID_ARGUMENT");
        assert_eq!(core.get_thread(&account, &thread.id).unwrap().folder, inbox);

        let other_account = core
            .add_account(
                &MailboxForm {
                    email: "other-cursor@example.test".into(),
                    imap_host: "imap.example.test".into(),
                    imap_port: 993,
                    imap_security: MailSecurity::Ssl,
                    smtp_host: "smtp.example.test".into(),
                    smtp_port: 465,
                    smtp_security: MailSecurity::Ssl,
                },
                "x",
                &LocalProbe,
            )
            .unwrap();
        let foreign_destination = core.create_folder(&other_account, "Elsewhere").unwrap();
        let foreign_folder = call_tool(
            &mut core,
            &Access::default(),
            "move_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.id.as_str(),"folder_id":foreign_destination.as_str()}),
        )
        .unwrap_err();
        assert_eq!(foreign_folder.code, "INVALID_ARGUMENT");
        assert_eq!(core.get_thread(&account, &thread.id).unwrap().folder, inbox);

        let response = call_tool(
            &mut core,
            &Access::default(),
            "move_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.id.as_str(),"folder_id":destination.as_str()}),
        )
        .unwrap();
        assert_eq!(response["queued"], true);
        assert_eq!(response["operation_ids"].as_array().unwrap().len(), 1);
        assert_eq!(
            core.get_thread(&account, &thread.id).unwrap().folder,
            destination
        );
    }

    #[test]
    fn move_message_propagates_the_core_missing_thread_error() {
        let (mut core, account, _) = seeded_cursor_core();
        let destination = core.create_folder(&account, "Projects").unwrap();
        let error = call_tool(
            &mut core,
            &Access::default(),
            "move_message",
            &json!({"account_id":account.as_str(),"thread_id":"missing-thread","folder_id":destination.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
    }

    #[test]
    fn move_message_refuses_an_account_outside_the_ceiling() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let error = call_tool(
            &mut core,
            &Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
            "move_message",
            &json!({"account_id":account.as_str(),"thread_id":"any-thread","folder_id":inbox.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
    }

    #[test]
    fn restore_message_cancels_only_the_current_pending_trash_without_exposing_operation_ids() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        core.dispatch(Command::Trash {
            account_id: account.clone(),
            thread_ids: vec![thread.id.clone()],
        })
        .unwrap();

        let response = call_tool(
            &mut core,
            &Access::default(),
            "restore_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.id.as_str()}),
        )
        .unwrap();

        assert_eq!(response, json!({"restored":true,"queued":false}));
        assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
        assert_eq!(core.queue_counts().unwrap().pending, 0);
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "restore_message");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn restore_message_after_ack_returns_only_the_causal_reverse_operation_id() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let original = core
            .dispatch_with_receipt(Command::Trash {
                account_id: account.clone(),
                thread_ids: vec![thread.id.clone()],
            })
            .unwrap()
            .operations[0]
            .operation_id
            .clone();
        let mut provider = AcceptProvider;
        assert!(matches!(
            core.tick(&mut provider).unwrap(),
            TickOutcome::Acked(id) if id == original
        ));

        let response = call_tool(
            &mut core,
            &Access::default(),
            "restore_message",
            &json!({"account_id":account.as_str(),"thread_id":thread.id.as_str()}),
        )
        .unwrap();

        assert_eq!(response["restored"], true);
        assert_eq!(response["queued"], true);
        assert_eq!(response.as_object().unwrap().len(), 3);
        assert!(response.get("thread_id").is_none());
        assert!(response["operation_id"].as_str().is_some());
        assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
        assert_eq!(core.queue_counts().unwrap().pending, 1);
    }

    #[test]
    fn restore_message_fails_closed_for_ceilings_and_unknown_or_non_current_trash() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        core.dispatch(Command::Trash {
            account_id: account.clone(),
            thread_ids: vec![thread.id.clone()],
        })
        .unwrap();
        let args = json!({"account_id":account.as_str(),"thread_id":thread.id.as_str()});
        let pending_before = core.queue_counts().unwrap().pending;

        for access in [
            Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            Access {
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "restore_message", &args).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert!(core.get_thread(&account, &thread.id).unwrap().deleted());
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        }

        let unknown_id = "unknown-restore-thread";
        let error = call_tool(
            &mut core,
            &Access::default(),
            "restore_message",
            &json!({"account_id":account.as_str(),"thread_id":unknown_id}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
        assert!(!error.message.contains(unknown_id));
        assert!(core.get_thread(&account, &thread.id).unwrap().deleted());
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        assert!(
            !format!("{:?}", core.list_mcp_audit(100).unwrap()).contains(unknown_id),
            "unknown ids must never reach the metadata-only audit"
        );
    }

    #[test]
    fn permanent_delete_requires_core_approval_then_queues_the_existing_command() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let pending_before = core.queue_counts().unwrap().pending;
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let args = json!({"account_id":account.as_str(),"thread_id":thread.id.as_str()});
        let access = Access {
            ceiling: PermissionLevel::Full,
            ..Access::default()
        };

        let pending = call_tool(&mut core, &access, "permanent_delete", &args).unwrap_err();
        assert_eq!(pending.code, "PERMISSION_DENIED");
        assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);

        let request_id = pending.pending_confirmation().unwrap();
        assert!(core
            .resolve_mcp_confirmation(request_id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        let response = call_tool(&mut core, &access, "permanent_delete", &args).unwrap();
        assert_eq!(response["queued"], true);
        assert_eq!(response["operation_ids"].as_array().unwrap().len(), 1);
        assert!(core.get_thread(&account, &thread.id).unwrap().deleted());
        assert_eq!(core.queue_counts().unwrap().pending, pending_before + 1);

        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "permanent_delete");
        assert_eq!(audit[0].outcome, "ok");
    }

    #[test]
    fn permanent_delete_default_or_insufficient_permission_fails_closed_without_an_operation() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let pending_before = core.queue_counts().unwrap().pending;
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let args = json!({"account_id":account.as_str(),"thread_id":thread.id.as_str()});

        for access in [
            Access::default(),
            Access {
                ceiling: PermissionLevel::Send,
                ..Access::default()
            },
        ] {
            let error = call_tool(&mut core, &access, "permanent_delete", &args).unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert!(error.pending_confirmation().is_none());
            assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
            assert_eq!(core.queue_counts().unwrap().pending, pending_before);
        }
    }

    #[test]
    fn permanent_delete_refuses_an_account_outside_the_ceiling_without_a_mutation() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let pending_before = core.queue_counts().unwrap().pending;
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let error = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Full,
                accounts: HashSet::from(["other-account".to_string()]),
                ..Access::default()
            },
            "permanent_delete",
            &json!({"account_id":account.as_str(),"thread_id":thread.id.as_str()}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
        assert!(error.pending_confirmation().is_none());
        assert!(!core.get_thread(&account, &thread.id).unwrap().deleted());
        assert_eq!(core.queue_counts().unwrap().pending, pending_before);
    }

    #[test]
    fn list_thread_messages_uses_core_and_returns_metadata_only() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let expected = core
            .list_thread_messages(&account, &thread.id)
            .unwrap()
            .into_iter()
            .map(|message| message.id.as_str().to_string())
            .collect::<Vec<_>>();

        let response = call_tool(
            &mut core,
            &Access::default(),
            "list_thread_messages",
            &json!({"account_id":account.as_str(),"thread_id":thread.id.as_str()}),
        )
        .unwrap();
        let messages = response["messages"].as_array().unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message["id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            expected
        );
        for message in messages {
            assert!(message.get("body").is_none());
            assert!(message.get("body_html").is_none());
            assert!(message.get("preview").is_none());
        }
    }

    #[test]
    fn get_messages_is_an_atomic_bounded_batch_of_the_single_message_projection() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let threads = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 200,
            })
            .unwrap()
            .threads;
        let ids = threads
            .iter()
            .map(|thread| {
                core.list_thread_messages(&account, &thread.id)
                    .unwrap()
                    .into_iter()
                    .next()
                    .unwrap()
                    .id
            })
            .collect::<Vec<_>>();
        assert!(ids.len() >= 2);
        let queue_before = core.queue_counts().unwrap();
        let read = Access {
            ceiling: PermissionLevel::Read,
            ..Access::default()
        };

        let batch = call_tool(
            &mut core,
            &read,
            "get_messages",
            &json!({"account_id":account.as_str(),"message_ids":ids.iter().map(|id| id.as_str()).collect::<Vec<_>>()}),
        )
        .unwrap();
        let batched = batch["messages"].as_array().unwrap();
        assert_eq!(batched.len(), ids.len());
        // Each row is exactly what the single-message door returns, in the
        // requested order -- no second projection and no reordering.
        for (position, id) in ids.iter().enumerate() {
            let single = call_tool(
                &mut core,
                &read,
                "get_message",
                &json!({"account_id":account.as_str(),"message_id":id.as_str()}),
            )
            .unwrap();
            assert_eq!(batched[position], single["message"]);
        }
        assert_eq!(core.queue_counts().unwrap(), queue_before);

        // One unknown id fails the whole read: a short array must never read
        // as "everything that exists".
        let mut with_unknown = ids.iter().map(|id| id.as_str()).collect::<Vec<_>>();
        with_unknown.push("mcp-test-unknown-message");
        let error = call_tool(
            &mut core,
            &read,
            "get_messages",
            &json!({"account_id":account.as_str(),"message_ids":with_unknown}),
        )
        .unwrap_err();
        assert_eq!(error.code, "MESSAGE_NOT_FOUND");
        assert!(!error.message.contains("mcp-test-unknown-message"));
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "get_messages");
        assert_eq!(audit[0].outcome, "denied_or_error");
        assert!(!format!("{:?}", audit[0]).contains("mcp-test-unknown-message"));

        // Bounds and duplicates fail closed before any Core read.
        for bad in [
            json!([]),
            json!([""]),
            json!([ids[0].as_str(), ids[0].as_str()]),
            json!((0..=MAX_BULK_THREAD_IDS)
                .map(|n| format!("id-{n}"))
                .collect::<Vec<_>>()),
        ] {
            let error = call_tool(
                &mut core,
                &read,
                "get_messages",
                &json!({"account_id":account.as_str(),"message_ids":bad}),
            )
            .unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT");
        }

        let denied = call_tool(
            &mut core,
            &Access {
                accounts: HashSet::from(["another-account".to_string()]),
                ..Access::default()
            },
            "get_messages",
            &json!({"account_id":account.as_str(),"message_ids":[ids[0].as_str()]}),
        )
        .unwrap_err();
        assert_eq!(denied.code, "PERMISSION_DENIED");
        assert_eq!(core.queue_counts().unwrap(), queue_before);

        let definition = tool_definitions()
            .into_iter()
            .find(|tool| tool["name"] == "get_messages")
            .unwrap();
        assert_eq!(
            definition["inputSchema"]["required"],
            json!(["account_id", "message_ids"])
        );
        assert_eq!(definition["inputSchema"]["additionalProperties"], false);
        assert_eq!(
            definition["inputSchema"]["properties"]["message_ids"]["maxItems"],
            json!(MAX_BULK_THREAD_IDS)
        );
        assert_eq!(
            definition["inputSchema"]["properties"]["message_ids"]["uniqueItems"],
            json!(true)
        );
    }

    #[test]
    fn get_message_uses_the_account_scoped_core_projection_without_hidden_fields() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox,
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let message = core
            .list_thread_messages(&account, &thread.id)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let queue_before = core.queue_counts().unwrap();

        let response = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            "get_message",
            &json!({"account_id":account.as_str(),"message_id":message.id.as_str()}),
        )
        .unwrap();
        let metadata = &response["message"];
        assert_eq!(metadata["id"], message.id.as_str());
        assert_eq!(metadata["thread_id"], message.thread_id.as_str());
        assert_eq!(metadata["folder_id"], message.folder.as_str());
        assert_eq!(metadata["subject"], message.subject);
        assert_eq!(core.queue_counts().unwrap(), queue_before);
        for hidden in [
            "account_id",
            "body",
            "body_html",
            "preview",
            "provider_uid",
            "message_id_header",
            "size_bytes",
        ] {
            assert!(
                metadata.get(hidden).is_none(),
                "get_message exposed hidden field {hidden}"
            );
        }
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].tool, "get_message");
        assert_eq!(audit[0].outcome, "ok");

        let denied = call_tool(
            &mut core,
            &Access {
                accounts: HashSet::from(["another-account".to_string()]),
                ..Access::default()
            },
            "get_message",
            &json!({"account_id":account.as_str(),"message_id":message.id.as_str()}),
        )
        .unwrap_err();
        assert_eq!(denied.code, "PERMISSION_DENIED");
        assert_eq!(core.queue_counts().unwrap(), queue_before);

        let definition = tool_definitions()
            .into_iter()
            .find(|tool| tool["name"] == "get_message")
            .unwrap();
        assert_eq!(
            definition["inputSchema"]["required"],
            json!(["account_id", "message_id"])
        );
        assert_eq!(definition["inputSchema"]["additionalProperties"], false);
        assert_eq!(
            definition["inputSchema"]["properties"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn search_mail_continues_the_seeded_core_page_without_overlap_or_skip() {
        let (mut core, account, _) = seeded_cursor_core();
        let plan = Query::parse("cursor").to_search_plan();
        let expected_ids = core
            .search(&account, &plan, None, 4)
            .unwrap()
            .threads
            .iter()
            .map(|thread| thread.id.as_str().to_string())
            .collect();

        assert_mcp_cursor_continuation(
            &mut core,
            "search_mail",
            json!({"account_id":account.as_str(),"query":"cursor","limit":2}),
            expected_ids,
        );
    }

    #[test]
    fn every_registered_tool_has_an_explicit_permission_mapping() {
        for tool in tool_definitions() {
            let name = tool["name"].as_str().unwrap();
            assert!(tool_policy(name).is_some(), "{name} is missing a policy");
            assert_eq!(canonical_audit_tool(name), name);
        }
        assert_eq!(canonical_audit_tool("untrusted tool"), "unknown");
    }

    #[test]
    fn capability_classes_follow_the_documented_r_d_w_m_h_matrix() {
        for (class, names) in [
            (
                CapabilityClass::Read,
                &[
                    "list_accounts",
                    "get_account",
                    "get_account_status",
                    "list_folders",
                    "get_folder_message_count",
                    "list_threads",
                    "list_snoozed",
                    "get_thread",
                    "list_thread_messages",
                    "get_message",
                    "search_mail",
                    "list_drafts",
                    "get_draft",
                    "list_attachments",
                    "get_attachment",
                    "download_attachment",
                    "save_attachment",
                    "list_draft_attachments",
                ][..],
            ),
            (
                CapabilityClass::Draft,
                &[
                    "create_draft",
                    "update_draft",
                    "delete_draft",
                    "reply_to_thread",
                    "forward_message",
                    "attach_file_to_draft",
                    "remove_attachment_from_draft",
                ][..],
            ),
            (
                CapabilityClass::Write,
                &[
                    "archive_message",
                    "bulk_archive",
                    "mark_read",
                    "bulk_mark_read",
                    "mark_unread",
                    "bulk_mark_unread",
                    "star_message",
                    "unstar_message",
                    "bulk_star",
                    "bulk_unstar",
                ][..],
            ),
            (
                CapabilityClass::Modify,
                &[
                    "snooze_message",
                    "bulk_snooze",
                    "move_message",
                    "bulk_move",
                    "restore_message",
                    "create_folder",
                ][..],
            ),
            (
                CapabilityClass::High,
                &[
                    "send_draft",
                    "send_email",
                    "delete_message",
                    "permanent_delete",
                    "bulk_delete",
                    "bulk_permanent_delete",
                ][..],
            ),
        ] {
            for name in names {
                assert_eq!(tool_policy(name).unwrap().capability, class, "{name}");
            }
        }
    }

    #[test]
    fn send_email_saves_the_reviewable_draft_and_sends_only_after_one_approval() {
        let (mut core, account, _) = seeded_cursor_core();
        assert!(core
            .set_mcp_client_permission_level("stdio", PermissionLevel::Send)
            .unwrap());
        let access = Access {
            ceiling: PermissionLevel::Send,
            ..Access::default()
        };
        let args = json!({
            "account_id": account.as_str(),
            "to": "recipient@example.test",
            "subject": "mcp send_email subject",
            "body": "mcp-test-body-marker",
        });
        let pending_before = core.queue_counts().unwrap().pending;

        let first = call_tool(&mut core, &access, "send_email", &args).unwrap_err();
        let request_id = first.pending_confirmation().unwrap();
        // The only thing queued before approval is the saved draft's own
        // ordinary sync operation, exactly as if the user had typed it in
        // Compose. No send is queued.
        let after_draft = core.queue_counts().unwrap().pending;
        assert_eq!(after_draft, pending_before + 1);

        // The exact message is a normal draft the user can read before
        // approving, and its body never comes back through MCP.
        let drafts = core.list_drafts(&account).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].to, "recipient@example.test");
        assert_eq!(drafts[0].subject, "mcp send_email subject");
        assert_eq!(drafts[0].body, "mcp-test-body-marker");
        // From is the account's own address, never a caller-chosen sender.
        let account_email = core
            .list_accounts()
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == account)
            .unwrap()
            .email;
        assert_eq!(drafts[0].from, account_email);

        // Repeating the same message reuses the same draft and the same
        // pending approval instead of piling up drafts.
        let again = call_tool(&mut core, &access, "send_email", &args).unwrap_err();
        assert_eq!(again.pending_confirmation(), Some(request_id));
        assert_eq!(core.list_drafts(&account).unwrap().len(), 1);

        // A different message is a different approval.
        let changed = call_tool(
            &mut core,
            &access,
            "send_email",
            &json!({
                "account_id": account.as_str(),
                "to": "recipient@example.test",
                "subject": "mcp send_email subject",
                "body": "a different message",
            }),
        )
        .unwrap_err();
        assert_ne!(changed.pending_confirmation(), Some(request_id));
        // A second draft, still no send.
        let after_second_draft = core.queue_counts().unwrap().pending;
        assert_eq!(after_second_draft, after_draft + 1);
        assert_eq!(core.list_drafts(&account).unwrap().len(), 2);

        assert!(core
            .resolve_mcp_confirmation(request_id, McpConfirmationChoice::AllowOnce)
            .unwrap());
        let response = call_tool(&mut core, &access, "send_email", &args).unwrap();
        assert_eq!(response["queued"], true);
        assert!(response["operation_id"].as_str().is_some());
        assert_no_submitted_sensitive_markers(&response);
        let after_send = core.queue_counts().unwrap().pending;
        assert_eq!(after_send, after_second_draft + 1);

        // Allow once is consumed: the next identical call needs a new approval
        // and does not queue a second send.
        let after = call_tool(&mut core, &access, "send_email", &args).unwrap_err();
        assert!(after.pending_confirmation().is_some());
        assert_eq!(core.queue_counts().unwrap().pending, after_send);
    }

    #[test]
    fn send_email_writes_no_draft_for_a_client_below_send() {
        let (mut core, account, _) = seeded_cursor_core();
        // Durable Settings policy below Send, even though the process ceiling
        // is Full: no local draft may appear.
        assert!(core
            .set_mcp_client_permission_level("stdio", PermissionLevel::Draft)
            .unwrap());
        let error = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Full,
                ..Access::default()
            },
            "send_email",
            &json!({"account_id":account.as_str(),"to":"recipient@example.test","body":"mcp-test-body-marker"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
        assert!(core.list_drafts(&account).unwrap().is_empty());

        // The process ceiling is a ceiling too.
        assert!(core
            .set_mcp_client_permission_level("stdio", PermissionLevel::Full)
            .unwrap());
        let error = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Draft,
                ..Access::default()
            },
            "send_email",
            &json!({"account_id":account.as_str(),"to":"recipient@example.test","body":"mcp-test-body-marker"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
        assert!(core.list_drafts(&account).unwrap().is_empty());

        // An account outside the ceiling is refused before any local write.
        let error = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Full,
                accounts: HashSet::from(["another-account".to_string()]),
                ..Access::default()
            },
            "send_email",
            &json!({"account_id":account.as_str(),"to":"recipient@example.test","body":"mcp-test-body-marker"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
        assert!(core.list_drafts(&account).unwrap().is_empty());
    }

    #[test]
    fn confirm_field_is_rejected_not_treated_as_authority() {
        let mut core = Core::memory().unwrap();
        core.set_mcp_enabled(1, true).unwrap();
        let access = Access {
            ceiling: PermissionLevel::Full,
            ..Access::default()
        };
        for (tool, args) in [
            (
                "send_draft",
                json!({"account_id":"acc1","draft_id":"d1","confirm":true}),
            ),
            (
                "send_email",
                json!({"account_id":"acc1","to":"recipient@example.test","confirm":true}),
            ),
            (
                "delete_message",
                json!({"account_id":"acc1","thread_id":"thread-1","confirm":true}),
            ),
            (
                "permanent_delete",
                json!({"account_id":"acc1","thread_id":"thread-1","confirm":true}),
            ),
            (
                "bulk_delete",
                json!({"account_id":"acc1","thread_ids":["thread-1"],"confirm":true}),
            ),
            (
                "bulk_permanent_delete",
                json!({"account_id":"acc1","thread_ids":["thread-1"],"confirm":true}),
            ),
        ] {
            let error = call_tool(&mut core, &access, tool, &args).unwrap_err();
            assert_eq!(error.code, "INVALID_ARGUMENT", "{tool}");
            assert!(error.pending_confirmation().is_none(), "{tool}");
        }
    }

    #[test]
    fn live_mcp_switch_is_checked_again_after_a_running_client_starts() {
        let mut core = Core::memory().unwrap();
        core.set_mcp_enabled(1, true).unwrap();
        assert!(call_tool(&mut core, &Access::default(), "list_accounts", &json!({})).is_ok());
        core.set_mcp_enabled(2, false).unwrap();
        let error =
            call_tool(&mut core, &Access::default(), "list_accounts", &json!({})).unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
    }

    #[test]
    fn account_environment_ceiling_denies_before_core_lookup() {
        let mut core = Core::memory().unwrap();
        core.set_mcp_enabled(1, true).unwrap();
        let error = call_tool(
            &mut core,
            &Access {
                accounts: HashSet::from(["allowed".to_string()]),
                ..Access::default()
            },
            "get_account",
            &json!({"account_id":"other"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
    }

    /// T-046/T-059: response drafts are editable Draft-level operations.
    /// A caller gets no confirmation field in its schema; send is a separate
    /// action that requires Feather Mail's own permission decision.
    #[test]
    fn response_tools_create_drafts_and_keep_send_separate() {
        let tools = tool_definitions();
        let reply = tools
            .iter()
            .find(|tool| tool["name"] == "reply_to_thread")
            .unwrap();
        assert_eq!(
            reply["inputSchema"]["required"],
            json!(["account_id", "thread_id"])
        );
        assert!(reply["inputSchema"]["properties"]
            .get("reply_all")
            .is_some());
        assert!(reply["description"]
            .as_str()
            .unwrap()
            .contains("send remains separate"));

        let forward = tools
            .iter()
            .find(|tool| tool["name"] == "forward_message")
            .unwrap();
        assert_eq!(
            forward["inputSchema"]["required"],
            json!(["account_id", "message_id"])
        );
        assert!(forward["inputSchema"]["properties"]
            .get("confirm")
            .is_none());
        assert!(forward["description"]
            .as_str()
            .unwrap()
            .contains("send remains separate"));
    }
    #[test]
    fn send_and_delete_fail_closed_when_mcp_is_off() {
        let mut core = Core::memory().unwrap();
        let access = Access::default();
        for (tool, args) in [
            ("send_draft", json!({"account_id":"foreign","draft_id":"d"})),
            (
                "delete_message",
                json!({"account_id":"foreign","thread_id":"t"}),
            ),
            (
                "bulk_delete",
                json!({"account_id":"foreign","thread_ids":["t"]}),
            ),
            (
                "bulk_permanent_delete",
                json!({"account_id":"foreign","thread_ids":["t"]}),
            ),
        ] {
            let err = call_tool(&mut core, &access, tool, &args).unwrap_err();
            assert_eq!(err.code, "PERMISSION_DENIED");
        }
    }
    #[test]
    fn attachment_path_requires_root() {
        let access = Access {
            ceiling: PermissionLevel::Draft,
            ..Access::default()
        };
        assert_eq!(
            safe_attachment_path(&access, "/etc/passwd")
                .unwrap_err()
                .code,
            "PERMISSION_DENIED"
        );
    }

    /// T-065b: exercise the public MCP doorway, rather than only its path
    /// helpers. No rejected caller path may reach Core's attachment mutation
    /// or add a queue operation.
    #[cfg(unix)]
    #[test]
    fn attachment_ingress_and_command_shaped_unknown_tool_fail_closed_at_runtime() {
        let (mut core, account, _) = seeded_cursor_core();
        let draft = core
            .save_draft(
                &account,
                None,
                DraftContent {
                    from: "sender@example.test".into(),
                    ..DraftContent::default()
                },
            )
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::Builder::new()
            .prefix("mcp-outside-")
            .tempdir_in(root.path().parent().unwrap())
            .unwrap();
        let outside_file = outside.path().join("not-allowed.txt");
        fs::write(&outside_file, b"fixture").unwrap();
        let traversal = root
            .path()
            .join("..")
            .join(outside.path().file_name().unwrap())
            .join("not-allowed.txt");
        assert!(traversal.is_file());
        let link = root.path().join("escape-link");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();

        let access = Access {
            attachment_root: Some(root.path().to_path_buf()),
            ..Access::default()
        };
        let queue_before = core.queue_counts().unwrap();
        for rejected_path in [&outside_file, &traversal, &link] {
            let error = call_tool(
                &mut core,
                &access,
                "attach_file_to_draft",
                &json!({
                    "account_id":account.as_str(),
                    "draft_id":draft.id.as_str(),
                    "path":rejected_path.to_string_lossy(),
                }),
            )
            .unwrap_err();
            assert_eq!(error.code, "PERMISSION_DENIED");
            assert!(core
                .list_draft_attachments(&account, &draft.id)
                .unwrap()
                .is_empty());
            assert_eq!(core.queue_counts().unwrap(), queue_before);
        }

        let error = call_tool(
            &mut core,
            &access,
            "run_shell_command",
            &json!({"command":"status"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "INVALID_ARGUMENT");
        assert!(core
            .list_draft_attachments(&account, &draft.id)
            .unwrap()
            .is_empty());
        assert_eq!(core.queue_counts().unwrap(), queue_before);
        let audit = core.list_mcp_audit(1).unwrap();
        assert_eq!(audit[0].client_id, "stdio");
        assert_eq!(audit[0].tool, "unknown");
        assert_eq!(audit[0].outcome, "denied_or_error");
        assert!(!format!("{:?}", audit[0]).contains("run_shell_command"));
    }

    #[test]
    fn incoming_attachment_tools_reject_a_non_allowlisted_account_before_lookup() {
        let mut core = Core::memory().unwrap();
        let access = Access {
            accounts: HashSet::from(["acc1".to_string()]),
            ..Access::default()
        };
        let error = call_tool(
            &mut core,
            &access,
            "get_attachment",
            &json!({"account_id":"acc2","attachment_id":"attachment:m1:0"}),
        )
        .unwrap_err();
        assert_eq!(error.code, "PERMISSION_DENIED");
    }

    #[test]
    fn incoming_attachment_export_is_bounded_to_safe_roots_and_streamed() {
        let cache = tempfile::tempdir().unwrap();
        let export_root = tempfile::tempdir().unwrap();
        let relative = PathBuf::from("a1/report.attachment");
        let source = cache.path().join(&relative);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"pdf").unwrap();
        let attachment = incoming_attachment(Some(relative));

        assert_eq!(
            cached_attachment_path_in(&attachment, cache.path()).unwrap(),
            source.canonicalize().unwrap()
        );
        let destination =
            safe_attachment_destination(export_root.path(), &export_root.path().join("saved.pdf"))
                .unwrap();
        export_cached_attachment(&attachment, cache.path(), &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"pdf");
        assert_eq!(
            safe_attachment_destination(export_root.path(), &destination)
                .unwrap_err()
                .code,
            "INVALID_ARGUMENT",
            "a Read-scoped export must not overwrite a pre-existing local file"
        );
    }

    #[test]
    fn incoming_attachment_paths_and_default_names_cannot_escape() {
        let cache = tempfile::tempdir().unwrap();
        let escaped = incoming_attachment(Some(PathBuf::from("../outside")));
        assert_eq!(
            cached_attachment_path_in(&escaped, cache.path())
                .unwrap_err()
                .code,
            "OPERATION_NOT_SUPPORTED"
        );

        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            safe_attachment_destination(root.path(), Path::new("relative.pdf"))
                .unwrap_err()
                .code,
            "INVALID_ARGUMENT"
        );
        assert_eq!(
            safe_attachment_destination(root.path(), Path::new("/tmp/outside.pdf"))
                .unwrap_err()
                .code,
            "PERMISSION_DENIED"
        );

        let safe = safe_attachment_filename("../../договор\u{0000}.pdf");
        assert_eq!(safe, PathBuf::from(".._.._договор_.pdf"));
        assert!(safe.to_string_lossy().len() <= 180);
        assert_eq!(safe_attachment_filename(".."), PathBuf::from("attachment"));
    }
    #[test]
    fn registered_tools_never_expose_credentials_or_shell() {
        let text = serde_json::to_string(&tool_definitions())
            .unwrap()
            .to_ascii_lowercase();
        for forbidden in ["password", "oauth", "token", "shell"] {
            assert!(
                !text.contains(forbidden),
                "forbidden MCP surface: {forbidden}"
            );
        }
    }

    #[test]
    fn metadata_projections_never_expose_message_or_draft_text() {
        let thread = Thread {
            id: ThreadId("thread-1".into()),
            account_id: AccountId("account-1".into()),
            folder: FolderId("folder-1".into()),
            from: Address {
                name: "Sender".into(),
                email: "sender@example.test".into(),
            },
            to: "recipient@example.test".into(),
            subject: "Subject metadata".into(),
            preview: String::new(),
            date: 1,
            placement: Placement::default(),
            starred: false,
            labels: Vec::new(),
            has_attachment: false,
            importance: Importance::Normal,
            message_count: 1,
            body_html: String::new(),
            message_id: None,
        };
        let draft = Draft {
            id: DraftId("draft-1".into()),
            account_id: AccountId("account-1".into()),
            thread_id: None,
            in_reply_to: None,
            from: "sender@example.test".into(),
            to: "recipient@example.test".into(),
            cc: String::new(),
            bcc: String::new(),
            subject: "Draft metadata".into(),
            body: String::new(),
            updated_at: 1,
            remote_uid: None,
        };

        for response in [thread_json(&thread), draft_metadata_json(&draft)] {
            assert!(response.get("body").is_none());
            assert!(response.get("preview").is_none());
        }
        assert_eq!(thread_json(&thread)["subject"], "Subject metadata");
        assert_eq!(draft_metadata_json(&draft)["subject"], "Draft metadata");
    }

    #[test]
    fn runtime_mcp_payloads_and_audit_never_echo_submitted_sensitive_markers() {
        let (mut core, account, inbox) = seeded_cursor_core();
        let thread = core
            .list_threads(ListThreadsQuery {
                account_id: account.clone(),
                folder_id: inbox.clone(),
                filter: ThreadFilter::All,
                after: None,
                limit: 1,
            })
            .unwrap()
            .threads
            .into_iter()
            .next()
            .unwrap();
        let message_id = thread.message_id.clone().unwrap();
        let body_cache = tempfile::tempdir().unwrap();
        core.store_body(
            &message_id,
            body_cache.path(),
            b"Content-Type: text/plain\r\n\r\nmcp-test-body-marker mcp-test-token-marker",
        )
        .unwrap();
        assert!(
            core.get_thread(&account, &thread.id)
                .unwrap()
                .preview
                .contains("mcp-test-body-marker"),
            "the runtime test must first prove Core holds the synthetic body"
        );

        let submitted_draft = json!({
            "account_id": account.as_str(),
            "from": "sender@example.test",
            "to": "recipient@example.test",
            "subject": "Synthetic metadata",
            "body": "mcp-test-body-marker mcp-test-password-marker mcp-test-token-marker",
            "password": "mcp-test-password-marker",
            "access_token": "mcp-test-token-marker",
        });
        let created = call_tool(
            &mut core,
            &Access::default(),
            "create_draft",
            &submitted_draft,
        )
        .unwrap();
        assert_no_submitted_sensitive_markers(&created);
        let draft_id = created["id"].as_str().unwrap().to_string();

        let fetched = call_tool(
            &mut core,
            &Access::default(),
            "get_draft",
            &json!({"account_id":account.as_str(),"draft_id":draft_id}),
        )
        .unwrap();
        let listed_drafts = call_tool(
            &mut core,
            &Access::default(),
            "list_drafts",
            &json!({"account_id":account.as_str()}),
        )
        .unwrap();
        let listed_threads = call_tool(
            &mut core,
            &Access::default(),
            "list_threads",
            &json!({"account_id":account.as_str(),"folder_id":inbox.as_str(),"limit":1}),
        )
        .unwrap();
        let fetched_thread = call_tool(
            &mut core,
            &Access::default(),
            "get_thread",
            &json!({"account_id":account.as_str(),"thread_id":thread.id.as_str()}),
        )
        .unwrap();
        let fetched_message = call_tool(
            &mut core,
            &Access::default(),
            "get_message",
            &json!({"account_id":account.as_str(),"message_id":message_id.as_str()}),
        )
        .unwrap();
        for response in [
            &fetched,
            &listed_drafts,
            &listed_threads,
            &fetched_thread,
            &fetched_message,
        ] {
            assert_no_submitted_sensitive_markers(response);
        }

        let denied = call_tool(
            &mut core,
            &Access {
                ceiling: PermissionLevel::Read,
                ..Access::default()
            },
            "create_draft",
            &submitted_draft,
        )
        .unwrap_err();
        assert_eq!(denied.code, "PERMISSION_DENIED");
        let denied_with_raw_identifiers = call_tool(
            &mut core,
            &Access {
                client_id: "mcp-test-token-marker".into(),
                accounts: HashSet::from(["allowed-account".to_string()]),
                ..Access::default()
            },
            "get_draft",
            &json!({
                "account_id":"mcp-test-password-marker",
                "draft_id":"mcp-test-body-marker",
            }),
        )
        .unwrap_err();
        assert_eq!(denied_with_raw_identifiers.code, "PERMISSION_DENIED");
        let unknown = call_tool(
            &mut core,
            &Access {
                client_id: "mcp-test-token-marker".into(),
                ..Access::default()
            },
            "mcp-test-body-marker",
            &json!({
                "account_id":"mcp-test-password-marker",
                "draft_id":"mcp-test-token-marker",
            }),
        )
        .unwrap_err();
        assert_eq!(unknown.code, "INVALID_ARGUMENT");
        for error in [&denied, &denied_with_raw_identifiers, &unknown] {
            for marker in [
                "mcp-test-body-marker",
                "mcp-test-password-marker",
                "mcp-test-token-marker",
            ] {
                assert!(
                    !error.message.contains(marker),
                    "MCP error leaked submitted marker: {marker}"
                );
            }
        }

        let audit = core.list_mcp_audit(100).unwrap();
        assert!(audit
            .iter()
            .any(|entry| { entry.tool == "create_draft" && entry.outcome == "denied_or_error" }));
        assert!(audit.iter().any(|entry| {
            entry.client_id == "unknown"
                && entry.tool == "get_draft"
                && entry.outcome == "denied_or_error"
        }));
        assert!(audit
            .iter()
            .any(|entry| entry.tool == "get_message" && entry.outcome == "ok"));
        assert!(audit.iter().any(|entry| {
            entry.client_id == "unknown"
                && entry.tool == "unknown"
                && entry.outcome == "denied_or_error"
        }));
        let persisted_metadata = format!("{audit:?}");
        for marker in [
            "mcp-test-body-marker",
            "mcp-test-password-marker",
            "mcp-test-token-marker",
        ] {
            assert!(
                !persisted_metadata.contains(marker),
                "persisted MCP audit metadata leaked submitted marker: {marker}"
            );
        }
    }

    #[test]
    fn every_registered_tool_is_documented() {
        let docs = include_str!("../../../docs/mcp/tools.md");
        for tool in tool_definitions() {
            let name = tool["name"].as_str().unwrap();
            assert!(
                docs.contains(&format!("`{name}`")),
                "undocumented tool: {name}"
            );
        }
    }
}
