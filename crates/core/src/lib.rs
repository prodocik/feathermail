//! Models, commands, policies, and the operation queue.
//!
//! UI, MCP, and shortcuts talk to Core. This crate does not import GTK.
//! The shell still reads [`fake::FakeMailStore`]; mail mutations go through [`Core`].

pub mod body;
pub mod command;
pub mod diagnostics;
pub mod error;
pub mod fake;
pub mod locator;
pub mod mailbox;
pub mod mcp;
pub mod model;
pub mod notifications;
pub mod preview;
pub mod provider;
pub mod queue;
pub mod remote;
pub mod search;
pub mod settings;
pub mod store;
pub mod sync_store;
pub mod threading;

pub use command::{Command, ListThreadsQuery, MailEvent, UnifiedThreadsQuery};
pub use diagnostics::{DiagnosticsSnapshot, McpAuditEntry};
pub use error::{CoreError, ErrorCode, ParseErrorCodeError};
pub use fake::FakeMailStore;
pub use mailbox::{
    account_id_from_email, display_name_from_email, normalize_image_domain,
    recipient_field_is_sendable, sender_image_domain, unique_account_id, AccountEdit,
    AddAccountError, MailSecurity, MailboxDraft, MailboxForm, MailboxFormError,
};
pub use mcp::{
    McpAuthorization, McpBulkHighRiskOutcome, McpClientSummary, McpConfirmationChoice,
    McpConfirmationRequest, McpFolderDeleteOutcome, McpPermissionLevel, McpSendOutcome,
};
pub use model::{
    empty_copy, folder_label_error, format_clock, group_label, stamp_headers, Account, AccountId,
    AccountStatus, Address, Attachment, AttachmentDownload, AttachmentEncoding, AttachmentId,
    BodyRef, CreateFolderError, DeleteFolderError, Density, Draft, DraftAttachment, DraftContent,
    DraftId, EmptyCopy, Folder, FolderId, FolderKind, FolderSummary, Importance, ListRow,
    MarkReadMode, Message, MessageId, OpKind, OpStatus, Operation, OperationId, OutboxMessage,
    OutgoingAttachment, Placement, RenameFolderError, ResponseKind, SyncState, Theme, Thread,
    ThreadCursor, ThreadFilter, ThreadId, ThreadMessage, ThreadPage, FIXTURE_NOW, LIST_PAGE,
    MAX_FOLDER_NAME_CHARS,
};
pub use notifications::NotificationCandidate;
pub use preview::{preview_from_raw_mime, DEFAULT_PREVIEW_CHARS};
pub use provider::{
    ApplyError, ConnectError, ConnectOk, MailConnector, MailProvider, Reauthenticate,
    ReauthingProvider, RemoteLocator, RemoteMessage, ARCHIVE_FOLDER_KEY, TRASH_FOLDER_KEY,
};
pub use queue::{retry_delay_secs, QueueCounts, TickOutcome};
pub use search::{IndexBatchResult, SearchResults, DEFAULT_INDEX_BATCH};
pub use settings::{
    Settings, SettingsStore, DEFAULT_CACHE_LIMIT_BYTES, DEFAULT_LIST_WIDTH, DEFAULT_SIDEBAR_WIDTH,
    MAX_LIST_WIDTH, MAX_SIDEBAR_WIDTH, MIN_LIST_WIDTH, MIN_SIDEBAR_WIDTH, SETTINGS_AUTOSAVE_MS,
};
pub use store::{
    AccountConnection, Core, DispatchReceipt, OperationReceipt, RemoveAccountReport, UndoReceipt,
    UndoTicket, FOLDER_PALETTE,
};
pub use sync_store::{CoreSyncStore, SyncProgress};
pub use threading::{assign_groups, ThreadHint, RETHREAD_SETTINGS_KEY};

/// Workspace probe so `cargo test -p feathermail-core` has a test.
pub fn crate_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::crate_name;

    #[test]
    fn crate_compiles() {
        assert!(crate_name().starts_with("feathermail-"));
    }
}
