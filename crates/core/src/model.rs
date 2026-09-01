//! Domain types: mailbox overlay, messages, drafts, queue, sync (T-006).

use std::fmt;
use std::path::PathBuf;

/// Calendar day of a Unix timestamp (UTC), Howard Hinnant's civil-from-days.
pub fn ymd_utc(unix: i64) -> (i32, u8, u8) {
    let z = unix.div_euclid(86_400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u8, d as u8)
}

/// Fixture clock: 2024-05-20 11:10:00 UTC (matches `ui-preview/` NOW).
pub const FIXTURE_NOW: i64 = 1_716_203_400;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ThreadId(pub String);

impl ThreadId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FolderId(pub String);

impl FolderId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AccountId(pub String);

impl AccountId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: AccountId,
    pub name: String,
    pub email: String,
    pub status: AccountStatus,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccountStatus {
    #[default]
    Synced,
    Syncing,
    Offline,
    Error,
}

impl AccountStatus {
    pub fn css_class(self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::Syncing => "syncing",
            Self::Offline => "offline",
            Self::Error => "error",
        }
    }

    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Synced => "Synced",
            Self::Syncing => "Syncing",
            Self::Offline => "Offline",
            Self::Error => "Sync issue",
        }
    }

    /// `accounts.status` column text (T-074). Same spelling as
    /// [`Self::css_class`] today; kept as its own method because the two
    /// concepts (DB status vs. CSS class) are free to diverge later.
    pub fn as_str(self) -> &'static str {
        self.css_class()
    }

    /// Round-trip of [`Self::as_str`]. Unrecognized text is not this enum's
    /// job to guess at — callers reading a row back fall back to a safe
    /// default themselves (T-074).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "synced" => Some(Self::Synced),
            "syncing" => Some(Self::Syncing),
            "offline" => Some(Self::Offline),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateFolderError {
    Empty,
    SystemName,
    Duplicate,
    /// NUL / CR / LF, or longer than IMAP will ever accept. Queuing this
    /// would only produce `create_failed` (T-116).
    InvalidName,
}

/// IMAP quoted mailbox names refuse NUL/CR/LF (`wire::imap_quote`), and a
/// several-thousand-character label is not a folder name anyone can use.
pub const MAX_FOLDER_NAME_CHARS: usize = 255;

/// `label` is already trimmed. `None` means Core may go on to the
/// system-name / duplicate checks.
pub fn folder_label_error(label: &str) -> Option<CreateFolderError> {
    if label.is_empty() {
        Some(CreateFolderError::Empty)
    } else if label.chars().count() > MAX_FOLDER_NAME_CHARS
        || label.bytes().any(|b| matches!(b, 0 | b'\r' | b'\n'))
    {
        Some(CreateFolderError::InvalidName)
    } else {
        None
    }
}

impl CreateFolderError {
    /// Toast text (T-074). Matches the wording `FakeMailStore`'s shell call
    /// sites already show for these three cases.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "Enter a folder name.",
            Self::SystemName => "That name is a system folder.",
            Self::Duplicate => "Folder already exists.",
            Self::InvalidName => "That folder name isn’t valid.",
        }
    }
}

/// T-060t: why a rename was refused. The three name-shaped cases share
/// [`CreateFolderError`]'s wording on purpose -- the user is being told the
/// same thing about the same field -- and the two identity-shaped ones are
/// new because creating a folder cannot hit them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameFolderError {
    Empty,
    SystemName,
    Duplicate,
    InvalidName,
    /// System folders are not renameable: their local `kind` is what
    /// resolves Archive/Trash operations to a mailbox, and the server owns
    /// their names through SPECIAL-USE.
    NotCustom,
    /// The folder exists only locally so far -- its `CreateFolder` has not
    /// been acked -- so there is no mailbox to rename yet. Saying so beats
    /// queueing a `RENAME` of a name the server has never heard of.
    NotOnServer,
}

impl RenameFolderError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "Enter a folder name.",
            Self::SystemName => "That name is a system folder.",
            Self::Duplicate => "Folder already exists.",
            Self::InvalidName => "That folder name isn’t valid.",
            Self::NotCustom => "System folders can’t be renamed.",
            Self::NotOnServer => "Wait until this folder reaches the server.",
        }
    }
}

/// T-060u: why [`Core::delete_folder`] refused.
///
/// Deleting a mailbox is the one folder operation that can destroy mail --
/// IMAP `DELETE` takes the messages with it -- so the refusals here are
/// deliberately blunt rather than clever. In particular there is no
/// "delete it and move the mail somewhere" mode: that would be a second,
/// silent bulk operation hidden inside a destructive one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteFolderError {
    /// System folders are not deletable: their local `kind` is what resolves
    /// Archive/Trash operations to a mailbox, and deleting the server's
    /// Sent or Trash would break the account, not tidy it.
    NotCustom,
    /// The folder still holds mail. Feather Mail will not decide for the
    /// user where that mail should go, and it will not throw it away as a
    /// side effect of tidying the sidebar.
    NotEmpty,
}

impl DeleteFolderError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotCustom => "System folders can’t be deleted.",
            Self::NotEmpty => "Move this folder’s mail out first.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FolderKind {
    Inbox,
    Starred,
    Snoozed,
    Sent,
    Drafts,
    Archive,
    Spam,
    Trash,
    Custom,
}

impl FolderKind {
    /// Sidebar order: system folders before custom ones (T-074, D21).
    /// T-108: the folders the unified mailbox shows, in sidebar order.
    /// Deliberately four and not eight -- the owner asked for "Входящие,
    /// Отправленные, Избранные, удалённые", and the four left out are ones
    /// whose meaning does not survive the merge: Drafts and Snoozed are
    /// per-account working state, Spam is per-provider, and Archive is
    /// where mail goes to stop being a list you read.
    pub const UNIFIED_ORDER: [Self; 4] = [Self::Inbox, Self::Sent, Self::Starred, Self::Trash];

    pub const SYSTEM_ORDER: [Self; 8] = [
        Self::Inbox,
        Self::Starred,
        Self::Snoozed,
        Self::Sent,
        Self::Drafts,
        Self::Archive,
        Self::Spam,
        Self::Trash,
    ];

    /// `folders.kind` column text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Starred => "starred",
            Self::Snoozed => "snoozed",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Archive => "archive",
            Self::Spam => "spam",
            Self::Trash => "trash",
            Self::Custom => "custom",
        }
    }

    /// Round-trip of [`Self::as_str`].
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "inbox" => Some(Self::Inbox),
            "starred" => Some(Self::Starred),
            "snoozed" => Some(Self::Snoozed),
            "sent" => Some(Self::Sent),
            "drafts" => Some(Self::Drafts),
            "archive" => Some(Self::Archive),
            "spam" => Some(Self::Spam),
            "trash" => Some(Self::Trash),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    /// Label shown when no real row backs this system folder yet (T-074:
    /// e.g. Sent/Drafts/Spam before the sync engine has created them).
    pub fn default_label(self) -> &'static str {
        match self {
            Self::Inbox => "Inbox",
            Self::Starred => "Starred",
            Self::Snoozed => "Snoozed",
            Self::Sent => "Sent",
            Self::Drafts => "Drafts",
            Self::Archive => "Archive",
            Self::Spam => "Spam",
            Self::Trash => "Trash",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: FolderId,
    pub label: String,
    pub kind: FolderKind,
    /// DESIGN.md folder-dot hex, only for custom folders.
    pub color: Option<&'static str>,
    /// System folders are shared names; custom folders belong to one account (D21).
    pub account_id: Option<AccountId>,
    /// T-084: true when this folder's `OpKind::CreateFolder` was rejected
    /// by the server non-retryably and never will exist there. See
    /// `crate::store::FOLDER_SUMMARY_SQL` for how this is derived --
    /// nothing is stored on `folders` itself, so a caller (sidebar) can
    /// show "not created on server" without a schema migration.
    pub create_failed: bool,
}

/// One sidebar row: a folder plus its D11 aggregate counts, as returned by
/// [`crate::Core::list_folders`] in a single query (T-074).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderSummary {
    pub folder: Folder,
    pub unread: u32,
    pub total: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub id: ThreadId,
    pub account_id: AccountId,
    pub folder: FolderId,
    pub from: Address,
    pub to: String,
    pub subject: String,
    pub preview: String,
    /// Unix seconds UTC.
    pub date: i64,
    /// Mailbox overlay. Unread is nested so trash cannot be unread (T-006).
    pub placement: Placement,
    pub starred: bool,
    pub labels: Vec<String>,
    pub has_attachment: bool,
    pub importance: Importance,
    /// Messages in the conversation. Row shows a count when > 1 (D22).
    pub message_count: u32,
    /// Fixture HTML until T-028; SQLite stores [`BodyRef`] paths, not BLOBs.
    pub body_html: String,
    /// The message whose body opening this thread would fetch/cache
    /// (T-024): the most recent message in the thread. `None` on rows this
    /// crate cannot (or does not need to) resolve a message for --
    /// Both list and open-thread projections carry this id from the newest
    /// account-scoped message. A thread with no message row yet is `None`,
    /// not an error -- there is simply no body to fetch. Full multi-message
    /// thread bodies are out of scope here; this is deliberately "the one
    /// message T-024's single-body cache can address," not a per-message
    /// list.
    pub message_id: Option<MessageId>,
}

/// Where a thread lives. Archive, snooze, and trash are mutually exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Active { unread: bool },
    Archived { unread: bool },
    Snoozed { until: i64, unread: bool },
    Trashed,
}

impl Placement {
    pub fn unread(self) -> bool {
        match self {
            Self::Active { unread } | Self::Archived { unread } | Self::Snoozed { unread, .. } => {
                unread
            }
            Self::Trashed => false,
        }
    }

    #[must_use]
    pub fn with_unread(self, unread: bool) -> Self {
        match self {
            Self::Active { .. } => Self::Active { unread },
            Self::Archived { .. } => Self::Archived { unread },
            Self::Snoozed { until, .. } => Self::Snoozed { until, unread },
            Self::Trashed => Self::Trashed,
        }
    }

    pub fn snoozed_until(self) -> Option<i64> {
        match self {
            Self::Snoozed { until, .. } => Some(until),
            _ => None,
        }
    }
}

impl Default for Placement {
    fn default() -> Self {
        Self::Active { unread: false }
    }
}

impl Thread {
    pub fn unread(&self) -> bool {
        self.placement.unread()
    }

    pub fn set_unread(&mut self, unread: bool) {
        self.placement = self.placement.with_unread(unread);
    }

    pub fn archived(&self) -> bool {
        matches!(self.placement, Placement::Archived { .. })
    }

    pub fn deleted(&self) -> bool {
        matches!(self.placement, Placement::Trashed)
    }

    pub fn snoozed_until(&self) -> Option<i64> {
        self.placement.snoozed_until()
    }

    pub fn archive(&mut self) {
        self.placement = Placement::Archived { unread: false };
    }

    pub fn trash(&mut self) {
        self.placement = Placement::Trashed;
    }

    pub fn snooze(&mut self, until: i64) {
        self.placement = Placement::Snoozed {
            until,
            unread: self.unread(),
        };
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Importance {
    Low,
    #[default]
    Normal,
    High,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MessageId(pub String);

impl MessageId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DraftId(pub String);

impl DraftId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DraftId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AttachmentId(pub String);

impl AttachmentId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OperationId(pub String);

impl OperationId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// On-disk body pointer (D13). FakeMailStore still inlines HTML on [`Thread::body_html`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyRef {
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub account_id: AccountId,
    pub thread_id: ThreadId,
    pub folder: FolderId,
    pub provider_uid: Option<u32>,
    pub message_id_header: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub date: i64,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub preview: String,
    /// Per-message IMAP `\Seen`. Trash lives on the thread ([`Placement::Trashed`]).
    pub unread: bool,
    pub starred: bool,
    pub has_attachment: bool,
    pub importance: Importance,
    pub body: Option<BodyRef>,
    pub size_bytes: u64,
}

/// One message in an opened thread (T-029). Metadata only — bodies stay
/// behind [`crate::Core::lookup_body`]. Debug is hand-written so a subject,
/// RFC 822 blob, or accidental `raw` field cannot leak into logs (D14).
#[derive(Clone, PartialEq, Eq)]
pub struct ThreadMessage {
    pub id: MessageId,
    pub account_id: AccountId,
    pub thread_id: ThreadId,
    pub folder: FolderId,
    pub provider_uid: Option<u32>,
    pub message_id_header: Option<String>,
    pub date: i64,
    pub from: Address,
    pub subject: String,
    pub unread: bool,
    pub starred: bool,
    pub has_attachment: bool,
    pub size_bytes: u64,
}

impl fmt::Debug for ThreadMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadMessage")
            .field("id", &self.id)
            .field("account_id", &self.account_id)
            .field("thread_id", &self.thread_id)
            .field("folder", &self.folder)
            .field("provider_uid", &self.provider_uid)
            .field("date", &self.date)
            .field("unread", &self.unread)
            .field("starred", &self.starred)
            .field("has_attachment", &self.has_attachment)
            .field("size_bytes", &self.size_bytes)
            .field(
                "from_len",
                &(self.from.name.len().saturating_add(self.from.email.len())),
            )
            .field("subject_len", &self.subject.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Draft {
    pub id: DraftId,
    pub account_id: AccountId,
    pub thread_id: Option<ThreadId>,
    pub in_reply_to: Option<MessageId>,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub updated_at: i64,
    pub remote_uid: Option<u32>,
}

impl fmt::Debug for Draft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Draft")
            .field("id", &self.id)
            .field("account_id", &self.account_id)
            .field("thread_id", &self.thread_id)
            .field("in_reply_to", &self.in_reply_to)
            .field("from", &self.from)
            .field("to_len", &self.to.len())
            .field("cc_len", &self.cc.len())
            .field("bcc_len", &self.bcc.len())
            .field("subject_len", &self.subject.len())
            .field("body_len", &self.body.len())
            .field("updated_at", &self.updated_at)
            .field("remote_uid", &self.remote_uid)
            .finish()
    }
}

/// Editable compose fields accepted by the Core draft door. Keeping this
/// separate from [`Draft`] means UI/MCP never manufacture ids, timestamps,
/// or remote state themselves.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DraftContent {
    pub thread_id: Option<ThreadId>,
    pub in_reply_to: Option<MessageId>,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
}

/// The source action for a locally created response draft (T-046). Kept in
/// Core's model so GTK shortcuts and MCP ask the same command to resolve
/// recipients and threading metadata from synchronized headers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseKind {
    Reply,
    ReplyAll,
    Forward,
}

impl fmt::Debug for DraftContent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DraftContent")
            .field("thread_id", &self.thread_id)
            .field("in_reply_to", &self.in_reply_to)
            .field("from", &self.from)
            .field("to_len", &self.to.len())
            .field("cc_len", &self.cc.len())
            .field("bcc_len", &self.bcc.len())
            .field("subject_len", &self.subject.len())
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Durable SMTP snapshot. The body is intentionally redacted from Debug so
/// queue/service diagnostics cannot accidentally log message contents (D14).
#[derive(Clone, PartialEq, Eq)]
pub struct OutboxMessage {
    pub id: String,
    pub account_id: AccountId,
    pub draft_id: Option<DraftId>,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
    pub attachments: Vec<OutgoingAttachment>,
    pub status: String,
}

impl fmt::Debug for OutboxMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboxMessage")
            .field("id", &self.id)
            .field("account_id", &self.account_id)
            .field("draft_id", &self.draft_id)
            .field("from", &self.from)
            .field("to_len", &self.to.len())
            .field("cc_len", &self.cc.len())
            .field("bcc_len", &self.bcc.len())
            .field("subject_len", &self.subject.len())
            .field("body_len", &self.body.len())
            .field("attachment_count", &self.attachments.len())
            .field("status", &self.status)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DraftAttachment {
    pub id: String,
    pub account_id: AccountId,
    pub draft_id: DraftId,
    pub filename: String,
    pub mime: String,
    pub size_bytes: u64,
    pub source_path: PathBuf,
}

impl fmt::Debug for DraftAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DraftAttachment")
            .field("id", &self.id)
            .field("account_id", &self.account_id)
            .field("draft_id", &self.draft_id)
            .field("filename", &self.filename)
            .field("mime", &self.mime)
            .field("size_bytes", &self.size_bytes)
            .field("source_path", &"[local file]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct OutgoingAttachment {
    pub filename: String,
    pub mime: String,
    pub size_bytes: u64,
    pub source_path: PathBuf,
}

impl fmt::Debug for OutgoingAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutgoingAttachment")
            .field("filename", &self.filename)
            .field("mime", &self.mime)
            .field("size_bytes", &self.size_bytes)
            .field("source_path", &"[local file]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Attachment {
    pub id: AttachmentId,
    pub account_id: AccountId,
    pub message_id: MessageId,
    pub filename: String,
    pub mime: String,
    pub size_bytes: u64,
    pub cache_path: Option<PathBuf>,
    pub content_id: Option<String>,
    /// IMAP payload section (`TEXT`, `1`, `2.1`, ...), populated once the
    /// cached RFC822 body has been parsed. `None` on legacy rows that have
    /// not been parsed since T-043.
    pub part_path: Option<String>,
    pub transfer_encoding: AttachmentEncoding,
}

impl fmt::Debug for Attachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attachment")
            .field("id", &self.id)
            .field("account_id", &self.account_id)
            .field("message_id", &self.message_id)
            .field("filename_bytes", &self.filename.len())
            .field("mime", &self.mime)
            .field("size_bytes", &self.size_bytes)
            .field("cached", &self.cache_path.is_some())
            .field("has_content_id", &self.content_id.is_some())
            .field("part_path", &self.part_path)
            .field("transfer_encoding", &self.transfer_encoding)
            .finish()
    }
}

/// How an incoming attachment section must be decoded after IMAP returns
/// it. Unknown values are never guessed: the attachment remains listable,
/// but Core/service reject downloading it rather than corrupting a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentEncoding {
    Base64,
    QuotedPrintable,
    Identity,
    Unsupported,
}

impl AttachmentEncoding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Base64 => "base64",
            Self::QuotedPrintable => "quoted-printable",
            Self::Identity => "identity",
            Self::Unsupported => "unsupported",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw {
            "base64" => Self::Base64,
            "quoted-printable" => Self::QuotedPrintable,
            "identity" => Self::Identity,
            _ => Self::Unsupported,
        }
    }
}

/// Everything the background service needs to download one attachment. This
/// is a Core data boundary: it contains no body bytes or credentials, and
/// prevents GTK/MCP from resolving IMAP folders or UIDs themselves.
#[derive(Clone, PartialEq, Eq)]
pub struct AttachmentDownload {
    pub attachment: Attachment,
    pub remote_folder: String,
    pub provider_uid: u32,
}

impl fmt::Debug for AttachmentDownload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttachmentDownload")
            .field("attachment", &self.attachment)
            .field("remote_folder_bytes", &self.remote_folder.len())
            .field("provider_uid", &self.provider_uid)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpKind {
    Archive,
    MarkRead,
    MarkUnread,
    Star,
    Unstar,
    Move,
    Trash,
    PermanentDelete,
    Snooze,
    Send,
    /// Uploads the current durable compose draft to the account's Drafts
    /// mailbox. The operation payload carries only the local revision;
    /// message content stays in `drafts` (T-042, D14).
    SyncDraft,
    CreateFolder,
    /// T-060t: renames one custom folder's mailbox on the server. The
    /// payload carries the exact source and destination mailbox names, both
    /// computed by Core before the operation is queued -- the applier never
    /// reconstructs a path, and `folders.remote_id` is rewritten only when
    /// the wire ACK lands.
    RenameFolder,
    /// T-060u: deletes one custom folder's mailbox on the server. The
    /// payload carries the exact mailbox name Core recorded when the user
    /// asked, so a later server-side rename cannot redirect the `DELETE`
    /// at a different mailbox. The applier refuses to delete a mailbox
    /// that is not empty on the wire, whatever the local mirror believed.
    DeleteFolder,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::MarkRead => "mark_read",
            Self::MarkUnread => "mark_unread",
            Self::Star => "star",
            Self::Unstar => "unstar",
            Self::Move => "move",
            Self::Trash => "trash",
            Self::PermanentDelete => "permanent_delete",
            Self::Snooze => "snooze",
            Self::Send => "send",
            Self::SyncDraft => "sync_draft",
            Self::CreateFolder => "create_folder",
            Self::RenameFolder => "rename_folder",
            Self::DeleteFolder => "delete_folder",
        }
    }
}

impl std::str::FromStr for OpKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "archive" => Ok(Self::Archive),
            "mark_read" => Ok(Self::MarkRead),
            "mark_unread" => Ok(Self::MarkUnread),
            "star" => Ok(Self::Star),
            "unstar" => Ok(Self::Unstar),
            "move" => Ok(Self::Move),
            "trash" => Ok(Self::Trash),
            "permanent_delete" => Ok(Self::PermanentDelete),
            "snooze" => Ok(Self::Snooze),
            "send" => Ok(Self::Send),
            "sync_draft" => Ok(Self::SyncDraft),
            "create_folder" => Ok(Self::CreateFolder),
            "rename_folder" => Ok(Self::RenameFolder),
            "delete_folder" => Ok(Self::DeleteFolder),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum OpStatus {
    #[default]
    Pending,
    Running,
    Acked,
    Failed,
    /// A reverse operation whose original operation has already been sent,
    /// but whose server outcome is not known yet. It is durable and must not
    /// be claimed by the provider until the causal predecessor ACKs.
    Blocked,
    /// Terminal local cancellation (used by Undo before wire apply, and by
    /// a reverse operation when its predecessor never reached the server).
    Cancelled,
    /// A durable Core-only mutation. Local operations (currently Snooze) are
    /// persisted for Undo/audit, but are never eligible for provider apply
    /// and never receive a wire ACK.
    Local,
}

impl OpStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Acked => "acked",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
            Self::Local => "local",
        }
    }
}

impl std::str::FromStr for OpStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "acked" => Ok(Self::Acked),
            "failed" => Ok(Self::Failed),
            "blocked" => Ok(Self::Blocked),
            "cancelled" => Ok(Self::Cancelled),
            "local" => Ok(Self::Local),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub id: OperationId,
    pub account_id: AccountId,
    pub target_id: String,
    pub kind: OpKind,
    pub payload: String,
    pub payload_hash: String,
    pub created_at: i64,
    pub retry_count: u32,
    pub next_attempt_at: Option<i64>,
    pub status: OpStatus,
    /// Causal predecessor for a reverse operation created by Undo. Kept on
    /// the queue row so restart/recovery never has to infer ordering from
    /// payloads or timestamps.
    pub undo_of: Option<OperationId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncState {
    pub account_id: AccountId,
    pub folder_id: FolderId,
    pub uidvalidity: Option<u32>,
    pub uidnext: Option<u32>,
    pub highest_modseq: Option<u64>,
    pub last_sync_at: Option<i64>,
}

/// First paint loads this many threads plus one neighbor page (T-013).
pub const LIST_PAGE: usize = 64;

/// Cursor into a list sorted by date DESC, then id DESC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadCursor {
    pub date: i64,
    pub id: ThreadId,
}

impl ThreadCursor {
    pub fn of(t: &Thread) -> Self {
        Self {
            date: t.date,
            id: t.id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadPage {
    pub threads: Vec<Thread>,
    pub next: Option<ThreadCursor>,
    pub prev: Option<ThreadCursor>,
    pub total: usize,
}

/// True if `t` is older than the cursor in DESC (date, id) order — later in the list.
pub fn older_than_cursor(t: &Thread, c: &ThreadCursor) -> bool {
    t.date < c.date || (t.date == c.date && t.id.as_str() < c.id.as_str())
}

/// Date headers for an already-sorted page. D38: not a full-scan of the mailbox.
pub fn stamp_headers<'a>(
    threads: impl IntoIterator<Item = &'a Thread>,
    now: i64,
    mut last: Option<&'static str>,
) -> (Vec<ListRow>, Option<&'static str>) {
    let mut rows = Vec::new();
    for t in threads {
        let group = group_label(t.date, now);
        if last != Some(group) {
            rows.push(ListRow::Header(group.into()));
            last = Some(group);
        }
        rows.push(ListRow::Thread(Box::new(t.clone())));
    }
    (rows, last)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
    System,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::Light | Self::System => Self::Dark,
            Self::Dark => Self::Light,
        }
    }

    pub fn resolve(self, prefer_dark: bool) -> Self {
        match self {
            Self::System => {
                if prefer_dark {
                    Self::Dark
                } else {
                    Self::Light
                }
            }
            other => other,
        }
    }
}

impl std::str::FromStr for Theme {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "light" => Ok(Self::Light),
            "dark" => Ok(Self::Dark),
            "system" => Ok(Self::System),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Density {
    Comfortable,
    Compact,
}

impl Density {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Comfortable => "comfortable",
            Self::Compact => "compact",
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::Comfortable => Self::Compact,
            Self::Compact => Self::Comfortable,
        }
    }
}

impl std::str::FromStr for Density {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "comfortable" => Ok(Self::Comfortable),
            "compact" => Ok(Self::Compact),
            _ => Err(()),
        }
    }
}

/// D27: immediate / after 2s / only explicit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MarkReadMode {
    #[default]
    Immediate,
    Delay,
    Manual,
}

impl MarkReadMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Delay => "delay",
            Self::Manual => "manual",
        }
    }
}

impl std::str::FromStr for MarkReadMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "immediate" => Ok(Self::Immediate),
            "delay" => Ok(Self::Delay),
            "manual" => Ok(Self::Manual),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadFilter {
    All,
    Unread,
    Starred,
    Attachments,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ListRow {
    Header(String),
    Thread(Box<Thread>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmptyCopy {
    pub title: String,
    pub body: String,
}

pub fn empty_copy(folder: &str, searching: bool) -> EmptyCopy {
    if searching {
        return EmptyCopy {
            title: "No messages found.".into(),
            body: "Try another search.".into(),
        };
    }
    match folder {
        "inbox" => EmptyCopy {
            title: "You're all caught up.".into(),
            body: "No new messages.".into(),
        },
        "sent" => EmptyCopy {
            title: "No sent messages yet.".into(),
            body: String::new(),
        },
        _ => EmptyCopy {
            title: "No messages.".into(),
            body: String::new(),
        },
    }
}

pub fn group_label(ts: i64, now: i64) -> &'static str {
    let delta = now.div_euclid(86_400) - ts.div_euclid(86_400);
    match delta {
        ..=0 => "Today",
        1 => "Yesterday",
        2..=6 => "This week",
        _ => "Older",
    }
}

pub fn format_clock(ts: i64, now: i64) -> String {
    match group_label(ts, now) {
        "Today" => {
            let sod = ts.rem_euclid(86_400) as u32;
            let h = sod / 3600;
            let m = (sod % 3600) / 60;
            let (h12, period) = match h {
                0 => (12, "AM"),
                1..=11 => (h, "AM"),
                12 => (12, "PM"),
                _ => (h - 12, "PM"),
            };
            format!("{h12}:{m:02} {period}")
        }
        "Yesterday" => "Yesterday".into(),
        _ => {
            const MONTHS: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            let (_y, m, d) = ymd_utc(ts);
            format!("{} {d}", MONTHS[(m.saturating_sub(1) as usize).min(11)])
        }
    }
}

pub fn plain_body(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            in_tag = true;
            continue;
        }
        if c == '>' {
            in_tag = false;
            continue;
        }
        if in_tag {
            continue;
        }
        if c == '&' {
            let mut ent = String::new();
            while let Some(&n) = chars.peek() {
                if n == ';' {
                    chars.next();
                    break;
                }
                if ent.len() > 8 {
                    break;
                }
                ent.push(n);
                chars.next();
            }
            match ent.as_str() {
                "amp" => out.push('&'),
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "nbsp" => out.push(' '),
                "quot" => out.push('"'),
                _ => {
                    out.push('&');
                    out.push_str(&ent);
                }
            }
            continue;
        }
        out.push(c);
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
}

pub fn in_folder(folder: &str, t: &Thread) -> bool {
    if t.deleted() {
        return folder == "trash";
    }
    if folder == "trash" {
        return false;
    }
    if folder == "archive" {
        return t.archived();
    }
    if t.archived() {
        return false;
    }
    if folder == "snoozed" {
        return t.snoozed_until().is_some();
    }
    if t.snoozed_until().is_some() {
        return false;
    }
    if folder == "starred" {
        return t.starred;
    }
    match folder {
        "inbox" => t.folder.as_str() == "inbox",
        "sent" => t.folder.as_str() == "sent",
        "drafts" => t.folder.as_str() == "drafts",
        "spam" => t.folder.as_str() == "spam",
        _ => t.labels.iter().any(|l| l.eq_ignore_ascii_case(folder)),
    }
}

pub fn matches_query(t: &Thread, raw: &str) -> bool {
    let q = raw.trim().to_ascii_lowercase();
    if q.is_empty() {
        return true;
    }
    let hay = format!(
        "{} {} {} {} {}",
        t.from.name, t.from.email, t.subject, t.preview, t.to
    )
    .to_ascii_lowercase();
    q.split_whitespace().all(|term| {
        if let Some(val) = term.strip_prefix("from:") {
            format!("{} {}", t.from.name, t.from.email)
                .to_ascii_lowercase()
                .contains(val)
        } else if let Some(val) = term.strip_prefix("subject:") {
            t.subject.to_ascii_lowercase().contains(val)
        } else if let Some(val) = term.strip_prefix("to:") {
            t.to.to_ascii_lowercase().contains(val)
        } else if let Some(val) = term.strip_prefix("is:") {
            match val {
                "unread" => t.unread(),
                "starred" => t.starred,
                "read" => !t.unread(),
                _ => hay.contains(term),
            }
        } else if let Some(val) = term.strip_prefix("has:") {
            val == "attachment" && t.has_attachment
        } else {
            hay.contains(term)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_now_is_20_may_2024() {
        assert_eq!(ymd_utc(FIXTURE_NOW), (2024, 5, 20));
    }

    #[test]
    fn today_clock() {
        assert_eq!(format_clock(FIXTURE_NOW, FIXTURE_NOW), "11:10 AM");
    }

    fn sample() -> Thread {
        Thread {
            id: ThreadId("x".into()),
            account_id: AccountId("john".into()),
            folder: FolderId("inbox".into()),
            from: Address {
                name: "A".into(),
                email: "a@example.com".into(),
            },
            to: "me".into(),
            subject: "s".into(),
            preview: "p".into(),
            date: FIXTURE_NOW,
            placement: Placement::Active { unread: false },
            starred: false,
            labels: vec!["Inbox".into()],
            has_attachment: false,
            importance: Importance::Normal,
            message_count: 1,
            body_html: String::new(),
            message_id: None,
        }
    }

    #[test]
    fn query_operators() {
        let mut t = sample();
        t.from = Address {
            name: "Notion Team".into(),
            email: "team@makenotion.com".into(),
        };
        t.subject = "Updates to our Terms of Service".into();
        t.preview = "We're updating".into();
        t.placement = Placement::Active { unread: true };
        t.has_attachment = true;
        assert!(matches_query(&t, "from:notion"));
        assert!(matches_query(&t, "is:unread"));
        assert!(matches_query(&t, "has:attachment"));
        assert!(!matches_query(&t, "is:starred"));
        assert!(matches_query(&t, "to:me"));
    }

    #[test]
    fn trashed_thread_cannot_be_unread() {
        let mut t = sample();
        t.set_unread(true);
        assert!(t.unread());
        t.trash();
        assert!(t.deleted());
        assert!(!t.unread());
        t.set_unread(true);
        assert!(!t.unread());
        assert_eq!(t.placement, Placement::Trashed);
    }

    #[test]
    fn placement_is_exclusive() {
        let mut t = sample();
        t.snooze(FIXTURE_NOW + 3600);
        assert!(t.snoozed_until().is_some());
        t.archive();
        assert!(t.archived());
        assert!(t.snoozed_until().is_none());
        assert!(!t.unread());
        t.trash();
        assert!(t.deleted());
        assert!(!t.archived());
        assert!(t.snoozed_until().is_none());
    }

    #[test]
    fn operation_kind_is_enum() {
        let op = Operation {
            id: OperationId("op-1".into()),
            account_id: AccountId("john".into()),
            target_id: "thread-1".into(),
            kind: OpKind::Archive,
            payload: "{}".into(),
            payload_hash: "hash".into(),
            created_at: FIXTURE_NOW,
            retry_count: 0,
            next_attempt_at: None,
            status: OpStatus::Pending,
            undo_of: None,
        };
        assert_eq!(op.kind.as_str(), "archive");
        assert_eq!(op.status.as_str(), "pending");
        let msg = Message {
            id: MessageId("m1".into()),
            account_id: AccountId("john".into()),
            thread_id: ThreadId("x".into()),
            folder: FolderId("inbox".into()),
            provider_uid: Some(42),
            message_id_header: Some("<a@b>".into()),
            in_reply_to: None,
            references: Vec::new(),
            date: FIXTURE_NOW,
            from: Address {
                name: "A".into(),
                email: "a@example.com".into(),
            },
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "s".into(),
            preview: "p".into(),
            unread: true,
            starred: false,
            has_attachment: false,
            importance: Importance::Normal,
            body: Some(BodyRef {
                path: PathBuf::from("/tmp/feathermail-body"),
                size_bytes: 12,
            }),
            size_bytes: 12,
        };
        assert!(msg.body.is_some());
        let draft = Draft {
            id: DraftId("d1".into()),
            account_id: AccountId("john".into()),
            thread_id: None,
            in_reply_to: None,
            from: "john.doe@example.com".into(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: String::new(),
            body: String::new(),
            updated_at: FIXTURE_NOW,
            remote_uid: None,
        };
        assert!(draft.body.is_empty());
        let att = Attachment {
            id: AttachmentId("a1".into()),
            account_id: AccountId("john".into()),
            message_id: MessageId("m1".into()),
            filename: "notes.pdf".into(),
            mime: "application/pdf".into(),
            size_bytes: 100,
            cache_path: None,
            content_id: None,
            part_path: Some("2".into()),
            transfer_encoding: AttachmentEncoding::Base64,
        };
        assert!(att.cache_path.is_none());
        let sync = SyncState {
            account_id: AccountId("john".into()),
            folder_id: FolderId("inbox".into()),
            uidvalidity: Some(1),
            uidnext: Some(10),
            highest_modseq: None,
            last_sync_at: Some(FIXTURE_NOW),
        };
        assert_eq!(sync.uidnext, Some(10));
    }

    #[test]
    fn today_yesterday_split_at_injected_midnight() {
        let midnight = 1_716_249_600; // 2024-05-21 00:00:00 UTC
        assert_eq!(ymd_utc(midnight), (2024, 5, 21));
        assert_eq!(group_label(midnight, midnight), "Today");
        assert_eq!(group_label(midnight - 1, midnight), "Yesterday");
        assert_eq!(group_label(midnight - 86_400, midnight), "Yesterday");
        assert_eq!(group_label(midnight - 86_400 - 1, midnight), "This week");
    }

    #[test]
    fn this_week_uses_civil_days() {
        assert_eq!(
            group_label(FIXTURE_NOW - 6 * 86_400, FIXTURE_NOW),
            "This week"
        );
        assert_eq!(group_label(FIXTURE_NOW - 7 * 86_400, FIXTURE_NOW), "Older");
    }

    #[test]
    fn stamp_headers_carry_avoids_duplicate_group() {
        let t = |id: &str, date: i64| {
            let mut row = sample();
            row.id = ThreadId(id.into());
            row.date = date;
            row
        };
        let a = t("a", FIXTURE_NOW);
        let b = t("b", FIXTURE_NOW - 60);
        let (rows, last) = stamp_headers([&a, &b], FIXTURE_NOW, None);
        assert!(matches!(rows.first(), Some(ListRow::Header(h)) if h == "Today"));
        assert_eq!(last, Some("Today"));
        let c = t("c", FIXTURE_NOW - 120);
        let (more, _) = stamp_headers([&c], FIXTURE_NOW, last);
        assert!(matches!(more.first(), Some(ListRow::Thread(_))));
    }

    #[test]
    fn empty_copy_matches_tz() {
        let inbox = empty_copy("inbox", false);
        assert_eq!(inbox.title, "You're all caught up.");
        assert_eq!(inbox.body, "No new messages.");
        let search = empty_copy("inbox", true);
        assert_eq!(search.title, "No messages found.");
        assert_eq!(search.body, "Try another search.");
        let sent = empty_copy("sent", false);
        assert_eq!(sent.title, "No sent messages yet.");
        assert!(sent.body.is_empty());
        let other = empty_copy("spam", false);
        assert_eq!(other.title, "No messages.");
        assert!(other.body.is_empty());
    }
}
