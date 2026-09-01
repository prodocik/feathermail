//! Commands, queries, and events for the Core bus (T-007, D9).

use crate::model::{AccountId, FolderId, FolderKind, ThreadCursor, ThreadFilter, ThreadId};

/// Mail mutations. Each dispatch writes SQLite and enqueues an [`crate::model::Operation`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Archive {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
    },
    Trash {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
    },
    /// Permanently delete messages on the provider, bypassing Trash (D28).
    /// This remains a queued high-risk operation so it works offline and
    /// uses the same provider boundary as every other mail mutation.
    PermanentDelete {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
    },
    MarkRead {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
    },
    MarkUnread {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
    },
    Star {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
    },
    Unstar {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
    },
    Snooze {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
        until: i64,
    },
    Move {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
        folder_id: FolderId,
    },
}

impl Command {
    pub fn account_id(&self) -> &AccountId {
        match self {
            Self::Archive { account_id, .. }
            | Self::Trash { account_id, .. }
            | Self::PermanentDelete { account_id, .. }
            | Self::MarkRead { account_id, .. }
            | Self::MarkUnread { account_id, .. }
            | Self::Star { account_id, .. }
            | Self::Unstar { account_id, .. }
            | Self::Snooze { account_id, .. }
            | Self::Move { account_id, .. } => account_id,
        }
    }

    pub fn thread_ids(&self) -> Vec<ThreadId> {
        match self {
            Self::Archive { thread_ids, .. }
            | Self::Trash { thread_ids, .. }
            | Self::PermanentDelete { thread_ids, .. }
            | Self::MarkRead { thread_ids, .. }
            | Self::MarkUnread { thread_ids, .. }
            | Self::Star { thread_ids, .. }
            | Self::Unstar { thread_ids, .. }
            | Self::Snooze { thread_ids, .. }
            | Self::Move { thread_ids, .. } => thread_ids.clone(),
        }
    }
}

/// Cursor page of threads in one account folder (including virtual Archive/Trash/Snoozed/Starred).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListThreadsQuery {
    pub account_id: AccountId,
    pub folder_id: FolderId,
    /// A mailbox-local display filter. This is part of the Core query (not
    /// a GTK post-filter), so the total and cursor describe the filtered
    /// result set even when its first matching thread is beyond page one.
    pub filter: ThreadFilter,
    pub after: Option<ThreadCursor>,
    pub limit: usize,
}

/// T-108: the same page, asked of every account at once. Not a variant of
/// [`ListThreadsQuery`] with an optional account: the unified mailbox has
/// no `folder_id` to name (each account has its own Inbox row), so it asks
/// by [`FolderKind`] instead, and only for the four kinds
/// [`FolderKind::UNIFIED_ORDER`] lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnifiedThreadsQuery {
    pub kind: FolderKind,
    pub filter: ThreadFilter,
    pub after: Option<ThreadCursor>,
    pub limit: usize,
}

/// UI and MCP subscribe to the same events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MailEvent {
    ThreadsChanged {
        account_id: AccountId,
        thread_ids: Vec<ThreadId>,
    },
}
